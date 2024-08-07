mod transformers;

use std::{
    collections::{hash_map, HashMap},
    hash::{Hash, Hasher},
    str::FromStr,
    sync::Arc,
};

use api_models::{
    admin as admin_api,
    enums::{self as api_enums, CountryAlpha2},
    payments::Address,
    routing::ConnectorSelection,
};
use diesel_models::enums as storage_enums;
use error_stack::ResultExt;
use euclid::{
    backend::{self, inputs as dsl_inputs, EuclidBackend},
    dssa::graph::{self as euclid_graph, CgraphExt},
    enums as euclid_enums,
    frontend::{ast, dir as euclid_dir},
};
use kgraph_utils::{
    mca as mca_graph,
    transformers::{IntoContext, IntoDirValue},
    types::CountryCurrencyFilter,
};
use masking::PeekInterface;
use rand::{
    distributions::{self, Distribution},
    SeedableRng,
};
use rustc_hash::FxHashMap;
use storage_impl::redis::cache::{CacheKey, CGRAPH_CACHE, ROUTING_CACHE};

#[cfg(feature = "payouts")]
use crate::core::payouts;
use crate::{
    core::{
        errors, errors as oss_errors, payments as payments_oss,
        routing::{self, helpers as routing_helpers},
    },
    logger,
    types::{
        api::{self, routing as routing_types},
        domain, storage as oss_storage,
        transformers::{ForeignFrom, ForeignInto, ForeignTryFrom},
    },
    utils::{OptionExt, ValueExt},
    SessionState,
};

pub enum CachedAlgorithm {
    Single(Box<routing_types::RoutableConnectorChoice>),
    Priority(Vec<routing_types::RoutableConnectorChoice>),
    VolumeSplit(Vec<routing_types::ConnectorVolumeSplit>),
    Advanced(backend::VirInterpreterBackend<ConnectorSelection>),
}

pub struct SessionFlowRoutingInput<'a> {
    pub state: &'a SessionState,
    pub country: Option<CountryAlpha2>,
    pub key_store: &'a domain::MerchantKeyStore,
    pub merchant_account: &'a domain::MerchantAccount,
    pub payment_attempt: &'a oss_storage::PaymentAttempt,
    pub payment_intent: &'a oss_storage::PaymentIntent,
    pub chosen: Vec<api::SessionConnectorData>,
}

pub struct SessionRoutingPmTypeInput<'a> {
    state: &'a SessionState,
    key_store: &'a domain::MerchantKeyStore,
    attempt_id: &'a str,
    routing_algorithm: &'a MerchantAccountRoutingAlgorithm,
    backend_input: dsl_inputs::BackendInput,
    allowed_connectors: FxHashMap<String, api::GetToken>,
    profile_id: Option<String>,
}

type RoutingResult<O> = oss_errors::CustomResult<O, errors::RoutingError>;

#[derive(Debug, serde::Serialize, serde::Deserialize)]
#[serde(untagged)]
enum MerchantAccountRoutingAlgorithm {
    V1(routing_types::RoutingAlgorithmRef),
}

impl Default for MerchantAccountRoutingAlgorithm {
    fn default() -> Self {
        Self::V1(routing_types::RoutingAlgorithmRef::default())
    }
}

#[cfg(feature = "payouts")]
pub fn make_dsl_input_for_payouts(
    payout_data: &payouts::PayoutData,
) -> RoutingResult<dsl_inputs::BackendInput> {
    let mandate = dsl_inputs::MandateData {
        mandate_acceptance_type: None,
        mandate_type: None,
        payment_type: None,
    };
    let metadata = payout_data
        .payouts
        .metadata
        .clone()
        .map(|val| val.parse_value("routing_parameters"))
        .transpose()
        .change_context(errors::RoutingError::MetadataParsingError)
        .attach_printable("Unable to parse routing_parameters from metadata of payouts")
        .unwrap_or(None);
    let payment = dsl_inputs::PaymentInput {
        amount: payout_data.payouts.amount,
        card_bin: None,
        currency: payout_data.payouts.destination_currency,
        authentication_type: None,
        capture_method: None,
        business_country: payout_data
            .payout_attempt
            .business_country
            .map(api_enums::Country::from_alpha2),
        billing_country: payout_data
            .billing_address
            .as_ref()
            .and_then(|bic| bic.country)
            .map(api_enums::Country::from_alpha2),
        business_label: payout_data.payout_attempt.business_label.clone(),
        setup_future_usage: None,
    };
    let payment_method = dsl_inputs::PaymentMethodInput {
        payment_method: payout_data
            .payouts
            .payout_type
            .map(api_enums::PaymentMethod::foreign_from),
        payment_method_type: payout_data
            .payout_method_data
            .clone()
            .map(api_enums::PaymentMethodType::foreign_from),
        card_network: None,
    };
    Ok(dsl_inputs::BackendInput {
        mandate,
        metadata,
        payment,
        payment_method,
    })
}

pub fn make_dsl_input<F>(
    payment_data: &payments_oss::PaymentData<F>,
) -> RoutingResult<dsl_inputs::BackendInput>
where
    F: Clone,
{
    let mandate_data = dsl_inputs::MandateData {
        mandate_acceptance_type: payment_data
            .setup_mandate
            .as_ref()
            .and_then(|mandate_data| {
                mandate_data
                    .customer_acceptance
                    .clone()
                    .map(|cat| match cat.acceptance_type {
                        hyperswitch_domain_models::mandates::AcceptanceType::Online => {
                            euclid_enums::MandateAcceptanceType::Online
                        }
                        hyperswitch_domain_models::mandates::AcceptanceType::Offline => {
                            euclid_enums::MandateAcceptanceType::Offline
                        }
                    })
            }),
        mandate_type: payment_data
            .setup_mandate
            .as_ref()
            .and_then(|mandate_data| {
                mandate_data.mandate_type.clone().map(|mt| match mt {
                    hyperswitch_domain_models::mandates::MandateDataType::SingleUse(_) => {
                        euclid_enums::MandateType::SingleUse
                    }
                    hyperswitch_domain_models::mandates::MandateDataType::MultiUse(_) => {
                        euclid_enums::MandateType::MultiUse
                    }
                })
            }),
        payment_type: Some(payment_data.setup_mandate.clone().map_or_else(
            || euclid_enums::PaymentType::NonMandate,
            |_| euclid_enums::PaymentType::SetupMandate,
        )),
    };
    let payment_method_input = dsl_inputs::PaymentMethodInput {
        payment_method: payment_data.payment_attempt.payment_method,
        payment_method_type: payment_data.payment_attempt.payment_method_type,
        card_network: payment_data
            .payment_method_data
            .as_ref()
            .and_then(|pm_data| match pm_data {
                api::PaymentMethodData::Card(card) => card.card_network.clone(),

                _ => None,
            }),
    };

    let payment_input = dsl_inputs::PaymentInput {
        amount: payment_data.payment_intent.amount,
        card_bin: payment_data
            .payment_method_data
            .as_ref()
            .and_then(|pm_data| match pm_data {
                api::PaymentMethodData::Card(card) => {
                    Some(card.card_number.peek().chars().take(6).collect())
                }
                _ => None,
            }),
        currency: payment_data.currency,
        authentication_type: payment_data.payment_attempt.authentication_type,
        capture_method: payment_data
            .payment_attempt
            .capture_method
            .and_then(|cm| cm.foreign_into()),
        business_country: payment_data
            .payment_intent
            .business_country
            .map(api_enums::Country::from_alpha2),
        billing_country: payment_data
            .address
            .get_payment_method_billing()
            .and_then(|bic| bic.address.as_ref())
            .and_then(|add| add.country)
            .map(api_enums::Country::from_alpha2),
        business_label: payment_data.payment_intent.business_label.clone(),
        setup_future_usage: payment_data.payment_intent.setup_future_usage,
    };

    let metadata = payment_data
        .payment_intent
        .metadata
        .clone()
        .map(|val| val.parse_value("routing_parameters"))
        .transpose()
        .change_context(errors::RoutingError::MetadataParsingError)
        .attach_printable("Unable to parse routing_parameters from metadata of payment_intent")
        .unwrap_or(None);

    Ok(dsl_inputs::BackendInput {
        metadata,
        payment: payment_input,
        payment_method: payment_method_input,
        mandate: mandate_data,
    })
}

pub async fn perform_static_routing_v1<F: Clone>(
    state: &SessionState,
    merchant_id: &common_utils::id_type::MerchantId,
    algorithm_ref: routing_types::RoutingAlgorithmRef,
    transaction_data: &routing::TransactionData<'_, F>,
) -> RoutingResult<Vec<routing_types::RoutableConnectorChoice>> {
    let profile_id = match transaction_data {
        routing::TransactionData::Payment(payment_data) => payment_data
            .payment_intent
            .profile_id
            .as_ref()
            .get_required_value("profile_id")
            .change_context(errors::RoutingError::ProfileIdMissing)?,
        #[cfg(feature = "payouts")]
        routing::TransactionData::Payout(payout_data) => &payout_data.payout_attempt.profile_id,
    };
    let algorithm_id = if let Some(id) = algorithm_ref.algorithm_id {
        id
    } else {
        let fallback_config = routing_helpers::get_merchant_default_config(
            &*state.clone().store,
            profile_id,
            &api_enums::TransactionType::from(transaction_data),
        )
        .await
        .change_context(errors::RoutingError::FallbackConfigFetchFailed)?;

        return Ok(fallback_config);
    };
    let cached_algorithm = ensure_algorithm_cached_v1(
        state,
        merchant_id,
        &algorithm_id,
        Some(profile_id).cloned(),
        &api_enums::TransactionType::from(transaction_data),
    )
    .await?;

    Ok(match cached_algorithm.as_ref() {
        CachedAlgorithm::Single(conn) => vec![(**conn).clone()],

        CachedAlgorithm::Priority(plist) => plist.clone(),

        CachedAlgorithm::VolumeSplit(splits) => perform_volume_split(splits.to_vec(), None)
            .change_context(errors::RoutingError::ConnectorSelectionFailed)?,

        CachedAlgorithm::Advanced(interpreter) => {
            let backend_input = match transaction_data {
                routing::TransactionData::Payment(payment_data) => make_dsl_input(payment_data)?,
                #[cfg(feature = "payouts")]
                routing::TransactionData::Payout(payout_data) => {
                    make_dsl_input_for_payouts(payout_data)?
                }
            };

            execute_dsl_and_get_connector_v1(backend_input, interpreter)?
        }
    })
}

async fn ensure_algorithm_cached_v1(
    state: &SessionState,
    merchant_id: &common_utils::id_type::MerchantId,
    algorithm_id: &str,
    profile_id: Option<String>,
    transaction_type: &api_enums::TransactionType,
) -> RoutingResult<Arc<CachedAlgorithm>> {
    let key = {
        let profile_id = profile_id
            .clone()
            .get_required_value("profile_id")
            .change_context(errors::RoutingError::ProfileIdMissing)?;

        match transaction_type {
            common_enums::TransactionType::Payment => {
                format!(
                    "routing_config_{}_{profile_id}",
                    merchant_id.get_string_repr()
                )
            }
            #[cfg(feature = "payouts")]
            common_enums::TransactionType::Payout => {
                format!(
                    "routing_config_po_{}_{profile_id}",
                    merchant_id.get_string_repr()
                )
            }
        }
    };

    let cached_algorithm = ROUTING_CACHE
        .get_val::<Arc<CachedAlgorithm>>(CacheKey {
            key: key.clone(),
            prefix: state.tenant.redis_key_prefix.clone(),
        })
        .await;

    let algorithm = if let Some(algo) = cached_algorithm {
        algo
    } else {
        refresh_routing_cache_v1(state, key.clone(), algorithm_id, profile_id).await?
    };

    Ok(algorithm)
}

pub fn perform_straight_through_routing(
    algorithm: &routing_types::StraightThroughAlgorithm,
    creds_identifier: Option<String>,
) -> RoutingResult<(Vec<routing_types::RoutableConnectorChoice>, bool)> {
    Ok(match algorithm {
        routing_types::StraightThroughAlgorithm::Single(conn) => {
            (vec![(**conn).clone()], creds_identifier.is_none())
        }

        routing_types::StraightThroughAlgorithm::Priority(conns) => (conns.clone(), true),

        routing_types::StraightThroughAlgorithm::VolumeSplit(splits) => (
            perform_volume_split(splits.to_vec(), None)
                .change_context(errors::RoutingError::ConnectorSelectionFailed)
                .attach_printable(
                    "Volume Split connector selection error in straight through routing",
                )?,
            true,
        ),
    })
}

fn execute_dsl_and_get_connector_v1(
    backend_input: dsl_inputs::BackendInput,
    interpreter: &backend::VirInterpreterBackend<ConnectorSelection>,
) -> RoutingResult<Vec<routing_types::RoutableConnectorChoice>> {
    let routing_output: routing_types::RoutingAlgorithm = interpreter
        .execute(backend_input)
        .map(|out| out.connector_selection.foreign_into())
        .change_context(errors::RoutingError::DslExecutionError)?;

    Ok(match routing_output {
        routing_types::RoutingAlgorithm::Priority(plist) => plist,

        routing_types::RoutingAlgorithm::VolumeSplit(splits) => perform_volume_split(splits, None)
            .change_context(errors::RoutingError::DslFinalConnectorSelectionFailed)?,

        _ => Err(errors::RoutingError::DslIncorrectSelectionAlgorithm)
            .attach_printable("Unsupported algorithm received as a result of static routing")?,
    })
}

pub async fn refresh_routing_cache_v1(
    state: &SessionState,
    key: String,
    algorithm_id: &str,
    profile_id: Option<String>,
) -> RoutingResult<Arc<CachedAlgorithm>> {
    let algorithm = {
        let algorithm = state
            .store
            .find_routing_algorithm_by_profile_id_algorithm_id(
                &profile_id.unwrap_or_default(),
                algorithm_id,
            )
            .await
            .change_context(errors::RoutingError::DslMissingInDb)?;
        let algorithm: routing_types::RoutingAlgorithm = algorithm
            .algorithm_data
            .parse_value("RoutingAlgorithm")
            .change_context(errors::RoutingError::DslParsingError)?;
        algorithm
    };

    let cached_algorithm = match algorithm {
        routing_types::RoutingAlgorithm::Single(conn) => CachedAlgorithm::Single(conn),
        routing_types::RoutingAlgorithm::Priority(plist) => CachedAlgorithm::Priority(plist),
        routing_types::RoutingAlgorithm::VolumeSplit(splits) => {
            CachedAlgorithm::VolumeSplit(splits)
        }
        routing_types::RoutingAlgorithm::Advanced(program) => {
            let interpreter = backend::VirInterpreterBackend::with_program(program)
                .change_context(errors::RoutingError::DslBackendInitError)
                .attach_printable("Error initializing DSL interpreter backend")?;

            CachedAlgorithm::Advanced(interpreter)
        }
    };

    let arc_cached_algorithm = Arc::new(cached_algorithm);

    ROUTING_CACHE
        .push(
            CacheKey {
                key,
                prefix: state.tenant.redis_key_prefix.clone(),
            },
            arc_cached_algorithm.clone(),
        )
        .await;

    Ok(arc_cached_algorithm)
}

pub fn perform_volume_split(
    mut splits: Vec<routing_types::ConnectorVolumeSplit>,
    rng_seed: Option<&str>,
) -> RoutingResult<Vec<routing_types::RoutableConnectorChoice>> {
    let weights: Vec<u8> = splits.iter().map(|sp| sp.split).collect();
    let weighted_index = distributions::WeightedIndex::new(weights)
        .change_context(errors::RoutingError::VolumeSplitFailed)
        .attach_printable("Error creating weighted distribution for volume split")?;

    let idx = if let Some(seed) = rng_seed {
        let mut hasher = hash_map::DefaultHasher::new();
        seed.hash(&mut hasher);
        let hash = hasher.finish();

        let mut rng = rand_chacha::ChaCha8Rng::seed_from_u64(hash);
        weighted_index.sample(&mut rng)
    } else {
        let mut rng = rand::thread_rng();
        weighted_index.sample(&mut rng)
    };

    splits
        .get(idx)
        .ok_or(errors::RoutingError::VolumeSplitFailed)
        .attach_printable("Volume split index lookup failed")?;

    // Panic Safety: We have performed a `get(idx)` operation just above which will
    // ensure that the index is always present, else throw an error.
    let removed = splits.remove(idx);
    splits.insert(0, removed);

    Ok(splits.into_iter().map(|sp| sp.connector).collect())
}

pub async fn get_merchant_cgraph<'a>(
    state: &SessionState,
    key_store: &domain::MerchantKeyStore,
    profile_id: Option<String>,
    transaction_type: &api_enums::TransactionType,
) -> RoutingResult<Arc<hyperswitch_constraint_graph::ConstraintGraph<euclid_dir::DirValue>>> {
    let merchant_id = &key_store.merchant_id;

    let key = {
        let profile_id = profile_id
            .clone()
            .get_required_value("profile_id")
            .change_context(errors::RoutingError::ProfileIdMissing)?;
        match transaction_type {
            api_enums::TransactionType::Payment => {
                format!("cgraph_{}_{}", merchant_id.get_string_repr(), profile_id)
            }
            #[cfg(feature = "payouts")]
            api_enums::TransactionType::Payout => {
                format!("cgraph_po_{}_{}", merchant_id.get_string_repr(), profile_id)
            }
        }
    };

    let cached_cgraph = CGRAPH_CACHE
        .get_val::<Arc<hyperswitch_constraint_graph::ConstraintGraph<euclid_dir::DirValue>>>(
            CacheKey {
                key: key.clone(),
                prefix: state.tenant.redis_key_prefix.clone(),
            },
        )
        .await;

    let cgraph = if let Some(graph) = cached_cgraph {
        graph
    } else {
        refresh_cgraph_cache(state, key_store, key.clone(), profile_id, transaction_type).await?
    };

    Ok(cgraph)
}

pub async fn refresh_cgraph_cache<'a>(
    state: &SessionState,
    key_store: &domain::MerchantKeyStore,
    key: String,
    profile_id: Option<String>,
    transaction_type: &api_enums::TransactionType,
) -> RoutingResult<Arc<hyperswitch_constraint_graph::ConstraintGraph<euclid_dir::DirValue>>> {
    let mut merchant_connector_accounts = state
        .store
        .find_merchant_connector_account_by_merchant_id_and_disabled_list(
            &state.into(),
            &key_store.merchant_id,
            false,
            key_store,
        )
        .await
        .change_context(errors::RoutingError::KgraphCacheRefreshFailed)?;

    match transaction_type {
        api_enums::TransactionType::Payment => {
            merchant_connector_accounts.retain(|mca| {
                mca.connector_type != storage_enums::ConnectorType::PaymentVas
                    && mca.connector_type != storage_enums::ConnectorType::PaymentMethodAuth
                    && mca.connector_type != storage_enums::ConnectorType::PayoutProcessor
                    && mca.connector_type != storage_enums::ConnectorType::AuthenticationProcessor
            });
        }
        #[cfg(feature = "payouts")]
        api_enums::TransactionType::Payout => {
            merchant_connector_accounts
                .retain(|mca| mca.connector_type == storage_enums::ConnectorType::PayoutProcessor);
        }
    };

    let connector_type = match transaction_type {
        api_enums::TransactionType::Payment => common_enums::ConnectorType::PaymentProcessor,
        #[cfg(feature = "payouts")]
        api_enums::TransactionType::Payout => common_enums::ConnectorType::PayoutProcessor,
    };

    let merchant_connector_accounts =
        payments_oss::helpers::filter_mca_based_on_profile_and_connector_type(
            merchant_connector_accounts,
            profile_id.as_ref(),
            connector_type,
        );

    let api_mcas = merchant_connector_accounts
        .into_iter()
        .map(admin_api::MerchantConnectorResponse::foreign_try_from)
        .collect::<Result<Vec<_>, _>>()
        .change_context(errors::RoutingError::KgraphCacheRefreshFailed)?;
    let connector_configs = state
        .conf
        .pm_filters
        .0
        .clone()
        .into_iter()
        .filter(|(key, _)| key != "default")
        .map(|(key, value)| {
            let key = api_enums::RoutableConnectors::from_str(&key)
                .map_err(|_| errors::RoutingError::InvalidConnectorName(key))?;

            Ok((key, value.foreign_into()))
        })
        .collect::<Result<HashMap<_, _>, errors::RoutingError>>()?;
    let default_configs = state
        .conf
        .pm_filters
        .0
        .get("default")
        .cloned()
        .map(ForeignFrom::foreign_from);
    let config_pm_filters = CountryCurrencyFilter {
        connector_configs,
        default_configs,
    };
    let cgraph = Arc::new(
        mca_graph::make_mca_graph(api_mcas, &config_pm_filters)
            .change_context(errors::RoutingError::KgraphCacheRefreshFailed)
            .attach_printable("when construction cgraph")?,
    );

    CGRAPH_CACHE
        .push(
            CacheKey {
                key,
                prefix: state.tenant.redis_key_prefix.clone(),
            },
            Arc::clone(&cgraph),
        )
        .await;

    Ok(cgraph)
}

#[allow(clippy::too_many_arguments)]
async fn perform_cgraph_filtering(
    state: &SessionState,
    key_store: &domain::MerchantKeyStore,
    chosen: Vec<routing_types::RoutableConnectorChoice>,
    backend_input: dsl_inputs::BackendInput,
    eligible_connectors: Option<&Vec<api_enums::RoutableConnectors>>,
    profile_id: Option<String>,
    transaction_type: &api_enums::TransactionType,
) -> RoutingResult<Vec<routing_types::RoutableConnectorChoice>> {
    let context = euclid_graph::AnalysisContext::from_dir_values(
        backend_input
            .into_context()
            .change_context(errors::RoutingError::KgraphAnalysisError)?,
    );
    let cached_cgraph = get_merchant_cgraph(state, key_store, profile_id, transaction_type).await?;

    let mut final_selection = Vec::<routing_types::RoutableConnectorChoice>::new();
    for choice in chosen {
        let routable_connector = choice.connector;
        let euclid_choice: ast::ConnectorChoice = choice.clone().foreign_into();
        let dir_val = euclid_choice
            .into_dir_value()
            .change_context(errors::RoutingError::KgraphAnalysisError)?;
        let cgraph_eligible = cached_cgraph
            .check_value_validity(
                dir_val,
                &context,
                &mut hyperswitch_constraint_graph::Memoization::new(),
                &mut hyperswitch_constraint_graph::CycleCheck::new(),
                None,
            )
            .change_context(errors::RoutingError::KgraphAnalysisError)?;

        let filter_eligible =
            eligible_connectors.map_or(true, |list| list.contains(&routable_connector));

        if cgraph_eligible && filter_eligible {
            final_selection.push(choice);
        }
    }

    Ok(final_selection)
}

pub async fn perform_eligibility_analysis<F: Clone>(
    state: &SessionState,
    key_store: &domain::MerchantKeyStore,
    chosen: Vec<routing_types::RoutableConnectorChoice>,
    transaction_data: &routing::TransactionData<'_, F>,
    eligible_connectors: Option<&Vec<api_enums::RoutableConnectors>>,
    profile_id: Option<String>,
) -> RoutingResult<Vec<routing_types::RoutableConnectorChoice>> {
    let backend_input = match transaction_data {
        routing::TransactionData::Payment(payment_data) => make_dsl_input(payment_data)?,
        #[cfg(feature = "payouts")]
        routing::TransactionData::Payout(payout_data) => make_dsl_input_for_payouts(payout_data)?,
    };

    perform_cgraph_filtering(
        state,
        key_store,
        chosen,
        backend_input,
        eligible_connectors,
        profile_id,
        &api_enums::TransactionType::from(transaction_data),
    )
    .await
}

pub async fn perform_fallback_routing<F: Clone>(
    state: &SessionState,
    key_store: &domain::MerchantKeyStore,
    transaction_data: &routing::TransactionData<'_, F>,
    eligible_connectors: Option<&Vec<api_enums::RoutableConnectors>>,
    profile_id: Option<String>,
) -> RoutingResult<Vec<routing_types::RoutableConnectorChoice>> {
    let fallback_config = routing_helpers::get_merchant_default_config(
        &*state.store,
        match transaction_data {
            routing::TransactionData::Payment(payment_data) => payment_data
                .payment_intent
                .profile_id
                .as_ref()
                .get_required_value("profile_id")
                .change_context(errors::RoutingError::ProfileIdMissing)?,
            #[cfg(feature = "payouts")]
            routing::TransactionData::Payout(payout_data) => &payout_data.payout_attempt.profile_id,
        },
        &api_enums::TransactionType::from(transaction_data),
    )
    .await
    .change_context(errors::RoutingError::FallbackConfigFetchFailed)?;

    let backend_input = match transaction_data {
        routing::TransactionData::Payment(payment_data) => make_dsl_input(payment_data)?,
        #[cfg(feature = "payouts")]
        routing::TransactionData::Payout(payout_data) => make_dsl_input_for_payouts(payout_data)?,
    };

    perform_cgraph_filtering(
        state,
        key_store,
        fallback_config,
        backend_input,
        eligible_connectors,
        profile_id,
        &api_enums::TransactionType::from(transaction_data),
    )
    .await
}

pub async fn perform_eligibility_analysis_with_fallback<F: Clone>(
    state: &SessionState,
    key_store: &domain::MerchantKeyStore,
    chosen: Vec<routing_types::RoutableConnectorChoice>,
    transaction_data: &routing::TransactionData<'_, F>,
    eligible_connectors: Option<Vec<api_enums::RoutableConnectors>>,
    profile_id: Option<String>,
) -> RoutingResult<Vec<routing_types::RoutableConnectorChoice>> {
    let mut final_selection = perform_eligibility_analysis(
        state,
        key_store,
        chosen,
        transaction_data,
        eligible_connectors.as_ref(),
        profile_id.clone(),
    )
    .await?;

    let fallback_selection = perform_fallback_routing(
        state,
        key_store,
        transaction_data,
        eligible_connectors.as_ref(),
        profile_id,
    )
    .await;

    final_selection.append(
        &mut fallback_selection
            .unwrap_or_default()
            .iter()
            .filter(|&routable_connector_choice| {
                !final_selection.contains(routable_connector_choice)
            })
            .cloned()
            .collect::<Vec<_>>(),
    );

    let final_selected_connectors = final_selection
        .iter()
        .map(|item| item.connector)
        .collect::<Vec<_>>();
    logger::debug!(final_selected_connectors_for_routing=?final_selected_connectors, "List of final selected connectors for routing");

    Ok(final_selection)
}

pub async fn perform_session_flow_routing(
    session_input: SessionFlowRoutingInput<'_>,
    transaction_type: &api_enums::TransactionType,
) -> RoutingResult<FxHashMap<api_enums::PaymentMethodType, Vec<routing_types::SessionRoutingChoice>>>
{
    let mut pm_type_map: FxHashMap<api_enums::PaymentMethodType, FxHashMap<String, api::GetToken>> =
        FxHashMap::default();

    let routing_algorithm: MerchantAccountRoutingAlgorithm = {
        let profile_id = session_input
            .payment_intent
            .profile_id
            .clone()
            .get_required_value("profile_id")
            .change_context(errors::RoutingError::ProfileIdMissing)?;

        let business_profile = session_input
            .state
            .store
            .find_business_profile_by_profile_id(&profile_id)
            .await
            .change_context(errors::RoutingError::ProfileNotFound)?;

        business_profile
            .routing_algorithm
            .clone()
            .map(|val| val.parse_value("MerchantAccountRoutingAlgorithm"))
            .transpose()
            .change_context(errors::RoutingError::InvalidRoutingAlgorithmStructure)?
            .unwrap_or_default()
    };

    let payment_method_input = dsl_inputs::PaymentMethodInput {
        payment_method: None,
        payment_method_type: None,
        card_network: None,
    };

    let payment_input = dsl_inputs::PaymentInput {
        amount: session_input.payment_intent.amount,
        currency: session_input
            .payment_intent
            .currency
            .get_required_value("Currency")
            .change_context(errors::RoutingError::DslMissingRequiredField {
                field_name: "currency".to_string(),
            })?,
        authentication_type: session_input.payment_attempt.authentication_type,
        card_bin: None,
        capture_method: session_input
            .payment_attempt
            .capture_method
            .and_then(|cm| cm.foreign_into()),
        business_country: session_input
            .payment_intent
            .business_country
            .map(api_enums::Country::from_alpha2),
        billing_country: session_input
            .country
            .map(storage_enums::Country::from_alpha2),
        business_label: session_input.payment_intent.business_label.clone(),
        setup_future_usage: session_input.payment_intent.setup_future_usage,
    };

    let metadata = session_input
        .payment_intent
        .metadata
        .clone()
        .map(|val| val.parse_value("routing_parameters"))
        .transpose()
        .change_context(errors::RoutingError::MetadataParsingError)
        .attach_printable("Unable to parse routing_parameters from metadata of payment_intent")
        .unwrap_or(None);

    let mut backend_input = dsl_inputs::BackendInput {
        metadata,
        payment: payment_input,
        payment_method: payment_method_input,
        mandate: dsl_inputs::MandateData {
            mandate_acceptance_type: None,
            mandate_type: None,
            payment_type: None,
        },
    };

    for connector_data in session_input.chosen.iter() {
        pm_type_map
            .entry(connector_data.payment_method_type)
            .or_default()
            .insert(
                connector_data.connector.connector_name.to_string(),
                connector_data.connector.get_token.clone(),
            );
    }

    let mut result: FxHashMap<
        api_enums::PaymentMethodType,
        Vec<routing_types::SessionRoutingChoice>,
    > = FxHashMap::default();

    for (pm_type, allowed_connectors) in pm_type_map {
        let euclid_pmt: euclid_enums::PaymentMethodType = pm_type;
        let euclid_pm: euclid_enums::PaymentMethod = euclid_pmt.into();

        backend_input.payment_method.payment_method = Some(euclid_pm);
        backend_input.payment_method.payment_method_type = Some(euclid_pmt);

        let session_pm_input = SessionRoutingPmTypeInput {
            state: session_input.state,
            key_store: session_input.key_store,
            attempt_id: &session_input.payment_attempt.attempt_id,
            routing_algorithm: &routing_algorithm,
            backend_input: backend_input.clone(),
            allowed_connectors,

            profile_id: session_input.payment_intent.profile_id.clone(),
        };
        let routable_connector_choice_option =
            perform_session_routing_for_pm_type(&session_pm_input, transaction_type).await?;

        if let Some(routable_connector_choice) = routable_connector_choice_option {
            let mut session_routing_choice: Vec<routing_types::SessionRoutingChoice> = Vec::new();

            for selection in routable_connector_choice {
                let connector_name = selection.connector.to_string();
                if let Some(get_token) = session_pm_input.allowed_connectors.get(&connector_name) {
                    let connector_data = api::ConnectorData::get_connector_by_name(
                        &session_pm_input.state.clone().conf.connectors,
                        &connector_name,
                        get_token.clone(),
                        selection.merchant_connector_id,
                    )
                    .change_context(errors::RoutingError::InvalidConnectorName(connector_name))?;

                    session_routing_choice.push(routing_types::SessionRoutingChoice {
                        connector: connector_data,
                        payment_method_type: pm_type,
                    });
                }
            }
            if !session_routing_choice.is_empty() {
                result.insert(pm_type, session_routing_choice);
            }
        }
    }

    Ok(result)
}

async fn perform_session_routing_for_pm_type(
    session_pm_input: &SessionRoutingPmTypeInput<'_>,
    transaction_type: &api_enums::TransactionType,
) -> RoutingResult<Option<Vec<api_models::routing::RoutableConnectorChoice>>> {
    let merchant_id = &session_pm_input.key_store.merchant_id;

    let chosen_connectors = match session_pm_input.routing_algorithm {
        MerchantAccountRoutingAlgorithm::V1(algorithm_ref) => {
            if let Some(ref algorithm_id) = algorithm_ref.algorithm_id {
                let cached_algorithm = ensure_algorithm_cached_v1(
                    &session_pm_input.state.clone(),
                    merchant_id,
                    algorithm_id,
                    session_pm_input.profile_id.clone(),
                    transaction_type,
                )
                .await?;

                match cached_algorithm.as_ref() {
                    CachedAlgorithm::Single(conn) => vec![(**conn).clone()],
                    CachedAlgorithm::Priority(plist) => plist.clone(),
                    CachedAlgorithm::VolumeSplit(splits) => {
                        perform_volume_split(splits.to_vec(), Some(session_pm_input.attempt_id))
                            .change_context(errors::RoutingError::ConnectorSelectionFailed)?
                    }
                    CachedAlgorithm::Advanced(interpreter) => execute_dsl_and_get_connector_v1(
                        session_pm_input.backend_input.clone(),
                        interpreter,
                    )?,
                }
            } else {
                routing_helpers::get_merchant_default_config(
                    &*session_pm_input.state.clone().store,
                    session_pm_input
                        .profile_id
                        .as_ref()
                        .get_required_value("profile_id")
                        .change_context(errors::RoutingError::ProfileIdMissing)?,
                    transaction_type,
                )
                .await
                .change_context(errors::RoutingError::FallbackConfigFetchFailed)?
            }
        }
    };

    let mut final_selection = perform_cgraph_filtering(
        &session_pm_input.state.clone(),
        session_pm_input.key_store,
        chosen_connectors,
        session_pm_input.backend_input.clone(),
        None,
        session_pm_input.profile_id.clone(),
        transaction_type,
    )
    .await?;

    if final_selection.is_empty() {
        let fallback = routing_helpers::get_merchant_default_config(
            &*session_pm_input.state.clone().store,
            session_pm_input
                .profile_id
                .as_ref()
                .get_required_value("profile_id")
                .change_context(errors::RoutingError::ProfileIdMissing)?,
            transaction_type,
        )
        .await
        .change_context(errors::RoutingError::FallbackConfigFetchFailed)?;

        final_selection = perform_cgraph_filtering(
            &session_pm_input.state.clone(),
            session_pm_input.key_store,
            fallback,
            session_pm_input.backend_input.clone(),
            None,
            session_pm_input.profile_id.clone(),
            transaction_type,
        )
        .await?;
    }

    if final_selection.is_empty() {
        Ok(None)
    } else {
        Ok(Some(final_selection))
    }
}

pub fn make_dsl_input_for_surcharge(
    payment_attempt: &oss_storage::PaymentAttempt,
    payment_intent: &oss_storage::PaymentIntent,
    billing_address: Option<Address>,
) -> RoutingResult<dsl_inputs::BackendInput> {
    let mandate_data = dsl_inputs::MandateData {
        mandate_acceptance_type: None,
        mandate_type: None,
        payment_type: None,
    };
    let payment_input = dsl_inputs::PaymentInput {
        amount: payment_attempt.amount,
        // currency is always populated in payment_attempt during payment create
        currency: payment_attempt
            .currency
            .get_required_value("currency")
            .change_context(errors::RoutingError::DslMissingRequiredField {
                field_name: "currency".to_string(),
            })?,
        authentication_type: payment_attempt.authentication_type,
        card_bin: None,
        capture_method: payment_attempt.capture_method,
        business_country: payment_intent
            .business_country
            .map(api_enums::Country::from_alpha2),
        billing_country: billing_address
            .and_then(|bic| bic.address)
            .and_then(|add| add.country)
            .map(api_enums::Country::from_alpha2),
        business_label: payment_intent.business_label.clone(),
        setup_future_usage: payment_intent.setup_future_usage,
    };
    let metadata = payment_intent
        .metadata
        .clone()
        .map(|val| val.parse_value("routing_parameters"))
        .transpose()
        .change_context(errors::RoutingError::MetadataParsingError)
        .attach_printable("Unable to parse routing_parameters from metadata of payment_intent")
        .unwrap_or(None);
    let payment_method_input = dsl_inputs::PaymentMethodInput {
        payment_method: None,
        payment_method_type: None,
        card_network: None,
    };
    let backend_input = dsl_inputs::BackendInput {
        metadata,
        payment: payment_input,
        payment_method: payment_method_input,
        mandate: mandate_data,
    };
    Ok(backend_input)
}
