use crate::logger::{log_debug, log_error, log_info, Logger};
use crate::types::{ChannelManager, KeysManager, LiquidityManager, PeerManager};
use crate::{Config, Error};

use chrono::{DateTime, Utc};
use lightning::events::HTLCDestination;
use lightning::ln::channelmanager::{InterceptId, MIN_FINAL_CLTV_EXPIRY_DELTA};
use lightning::ln::msgs::SocketAddress;
use lightning::ln::{ChannelId, PaymentHash};
use lightning::routing::router::{RouteHint, RouteHintHop};
use lightning_invoice::{Bolt11Invoice, InvoiceBuilder, RoutingFees};
use lightning_liquidity::events::Event;
use lightning_liquidity::lsps0::ser::RequestId;
use lightning_liquidity::lsps2::event::{LSPS2ClientEvent, LSPS2ServiceEvent};
use lightning_liquidity::lsps2::msgs::{OpeningFeeParams, RawOpeningFeeParams};
use lightning_liquidity::lsps2::utils::compute_opening_fee;

use bitcoin::hashes::{sha256, Hash};
use bitcoin::secp256k1::{PublicKey, Secp256k1};

use tokio::sync::oneshot;

use std::collections::HashMap;
use std::ops::Deref;
use std::sync::{Arc, Mutex};
use std::time::Duration;

const LIQUIDITY_REQUEST_TIMEOUT_SECS: u64 = 5;

struct LSPS2Service {
	address: SocketAddress,
	node_id: PublicKey,
	token: Option<String>,
	pending_fee_requests: Mutex<HashMap<RequestId, oneshot::Sender<LSPS2FeeResponse>>>,
	pending_buy_requests: Mutex<HashMap<RequestId, oneshot::Sender<LSPS2BuyResponse>>>,
}

pub(crate) struct LiquiditySource<L: Deref>
where
	L::Target: Logger,
{
	lsps2_service: Option<LSPS2Service>,
	channel_manager: Arc<ChannelManager>,
	keys_manager: Arc<KeysManager>,
	liquidity_manager: Arc<LiquidityManager>,
	config: Arc<Config>,
	logger: L,
}

impl<L: Deref> LiquiditySource<L>
where
	L::Target: Logger,
{
	pub(crate) fn new_lsps2(
		address: SocketAddress, node_id: PublicKey, token: Option<String>,
		channel_manager: Arc<ChannelManager>, keys_manager: Arc<KeysManager>,
		liquidity_manager: Arc<LiquidityManager>, config: Arc<Config>, logger: L,
	) -> Self {
		let pending_fee_requests = Mutex::new(HashMap::new());
		let pending_buy_requests = Mutex::new(HashMap::new());
		let lsps2_service = Some(LSPS2Service {
			address,
			node_id,
			token,
			pending_fee_requests,
			pending_buy_requests,
		});
		Self { lsps2_service, channel_manager, keys_manager, liquidity_manager, config, logger }
	}

	pub(crate) fn new_lsps2_provider(
		channel_manager: Arc<ChannelManager>, keys_manager: Arc<KeysManager>,
		liquidity_manager: Arc<LiquidityManager>, config: Arc<Config>, logger: L,
	) -> Self {
		Self {
			lsps2_service: None,
			channel_manager,
			keys_manager,
			liquidity_manager,
			config,
			logger,
		}
	}

	pub(crate) fn channel_ready(
		&self, user_channel_id: u128, channel_id: &ChannelId, counterparty_node_id: &PublicKey,
	) {
		if let Some(lsps2_service_handler) = self.liquidity_manager.lsps2_service_handler() {
			if let Err(e) = lsps2_service_handler.channel_ready(
				user_channel_id,
				channel_id,
				counterparty_node_id,
			) {
				log_error!(self.logger, "Errored processing htlc handling failed event: {:?}", e);
			}
		}
	}

	pub(crate) fn htlc_intercepted(
		&self, intercept_scid: u64, intercept_id: InterceptId, expected_outbound_amount_msat: u64,
		payment_hash: PaymentHash,
	) {
		if let Some(lsps2_service_handler) = self.liquidity_manager.lsps2_service_handler() {
			if let Err(e) = lsps2_service_handler.htlc_intercepted(
				intercept_scid,
				intercept_id,
				expected_outbound_amount_msat,
				payment_hash,
			) {
				log_error!(self.logger, "Failed to handle intercepted HTLC: {:?}", e);
			}
		}
	}

	pub(crate) fn htlc_handling_failed(&self, failed_next_destination: HTLCDestination) {
		if let Some(lsps2_service_handler) = self.liquidity_manager.lsps2_service_handler() {
			if let Err(e) = lsps2_service_handler.htlc_handling_failed(failed_next_destination) {
				log_error!(self.logger, "Errored processing htlc handling failed event: {:?}", e);
			}
		}
	}

	pub(crate) fn payment_forwarded(&self, next_channel_id: Option<ChannelId>) {
		if let Some(next_channel_id) = next_channel_id {
			if let Some(lsps2_service_handler) = self.liquidity_manager.lsps2_service_handler() {
				if let Err(e) = lsps2_service_handler.payment_forwarded(next_channel_id) {
					log_error!(self.logger, "Failed to handle payment forwarded: {:?}", e);
				}
			}
		}
	}

	pub(crate) fn set_peer_manager(&self, peer_manager: Arc<PeerManager>) {
		let process_msgs_callback = move || peer_manager.process_events();
		self.liquidity_manager.set_process_msgs_callback(process_msgs_callback);
	}

	pub(crate) fn liquidity_manager(&self) -> &LiquidityManager {
		self.liquidity_manager.as_ref()
	}

	pub(crate) fn get_liquidity_source_details(&self) -> Option<(PublicKey, SocketAddress)> {
		self.lsps2_service.as_ref().map(|s| (s.node_id, s.address.clone()))
	}

	pub(crate) async fn handle_next_event(&self) {
		match self.liquidity_manager.next_event_async().await {
			Event::LSPS2Service(LSPS2ServiceEvent::GetInfo {
				request_id,
				counterparty_node_id,
				token: _,
			}) => {
				let service_handler = self.liquidity_manager.lsps2_service_handler().unwrap();

				let min_fee_msat = 0;
				let proportional = 0;
				let mut valid_until: DateTime<Utc> = Utc::now();
				valid_until += chrono::Duration::minutes(10);
				let min_lifetime = 1008;
				let max_client_to_self_delay = 144;
				let min_payment_size_msat = 1000;
				let max_payment_size_msat = 10_000_000_000;

				let opening_fee_params = RawOpeningFeeParams {
					min_fee_msat,
					proportional,
					valid_until,
					min_lifetime,
					max_client_to_self_delay,
					min_payment_size_msat,
					max_payment_size_msat,
				};

				let opening_fee_params_menu = vec![opening_fee_params];

				if let Err(e) = service_handler.opening_fee_params_generated(
					&counterparty_node_id,
					request_id,
					opening_fee_params_menu,
				) {
					log_error!(
						self.logger,
						"Failed to handle generated opening fee params: {:?}",
						e
					);
				}
			},
			Event::LSPS2Service(LSPS2ServiceEvent::BuyRequest {
				request_id,
				counterparty_node_id,
				opening_fee_params: _,
				payment_size_msat: _,
			}) => {
				let user_channel_id = 0;
				let scid = self.channel_manager.get_intercept_scid();
				let cltv_expiry_delta = 72;
				let client_trusts_lsp = true;

				let lsps2_service_handler = self
					.liquidity_manager
					.lsps2_service_handler()
					.expect("to be configured with lsps2 service config");

				if let Err(e) = lsps2_service_handler.invoice_parameters_generated(
					&counterparty_node_id,
					request_id,
					scid,
					cltv_expiry_delta,
					client_trusts_lsp,
					user_channel_id,
				) {
					log_error!(self.logger, "Failed to provide invoice parameters: {:?}", e);
				}
			},
			Event::LSPS2Service(LSPS2ServiceEvent::OpenChannel {
				their_network_key,
				amt_to_forward_msat,
				opening_fee_msat: _,
				user_channel_id,
				intercept_scid: _,
			}) => {
				let channel_size_sats = (amt_to_forward_msat / 1000) * 4;
				let mut config = *self.channel_manager.get_current_default_configuration();
				config
					.channel_handshake_config
					.max_inbound_htlc_value_in_flight_percent_of_channel = 100;
				config.channel_config.forwarding_fee_base_msat = 0;
				config.channel_config.forwarding_fee_proportional_millionths = 0;

				if let Err(e) = self.channel_manager.create_channel(
					their_network_key,
					channel_size_sats,
					0,
					user_channel_id,
					None,
					Some(config),
				) {
					log_error!(self.logger, "Failed to open jit channel: {:?}", e);
				}
			},
			Event::LSPS2Client(LSPS2ClientEvent::OpeningParametersReady {
				request_id,
				counterparty_node_id,
				opening_fee_params_menu,
			}) => {
				if let Some(lsps2_service) = self.lsps2_service.as_ref() {
					if counterparty_node_id != lsps2_service.node_id {
						debug_assert!(
							false,
							"Received response from unexpected LSP counterparty. This should never happen."
							);
						log_error!(
							self.logger,
							"Received response from unexpected LSP counterparty. This should never happen."
							);
						return;
					}

					if let Some(sender) =
						lsps2_service.pending_fee_requests.lock().unwrap().remove(&request_id)
					{
						let response = LSPS2FeeResponse { opening_fee_params_menu };

						match sender.send(response) {
							Ok(()) => (),
							Err(e) => {
								log_error!(
									self.logger,
									"Failed to handle response from liquidity service: {:?}",
									e
								);
							},
						}
					} else {
						debug_assert!(
							false,
							"Received response from liquidity service for unknown request."
						);
						log_error!(
							self.logger,
							"Received response from liquidity service for unknown request."
						);
					}
				} else {
					log_error!(
						self.logger,
						"Received unexpected LSPS2Client::OpeningParametersReady event!"
					);
				}
			},
			Event::LSPS2Client(LSPS2ClientEvent::InvoiceParametersReady {
				request_id,
				counterparty_node_id,
				intercept_scid,
				cltv_expiry_delta,
				..
			}) => {
				if let Some(lsps2_service) = self.lsps2_service.as_ref() {
					if counterparty_node_id != lsps2_service.node_id {
						debug_assert!(
							false,
							"Received response from unexpected LSP counterparty. This should never happen."
						);
						log_error!(
							self.logger,
							"Received response from unexpected LSP counterparty. This should never happen."
						);
						return;
					}

					if let Some(sender) =
						lsps2_service.pending_buy_requests.lock().unwrap().remove(&request_id)
					{
						let response = LSPS2BuyResponse { intercept_scid, cltv_expiry_delta };

						match sender.send(response) {
							Ok(()) => (),
							Err(e) => {
								log_error!(
									self.logger,
									"Failed to handle response from liquidity service: {:?}",
									e
								);
							},
						}
					} else {
						debug_assert!(
							false,
							"Received response from liquidity service for unknown request."
						);
						log_error!(
							self.logger,
							"Received response from liquidity service for unknown request."
						);
					}
				} else {
					log_error!(
						self.logger,
						"Received unexpected LSPS2Client::InvoiceParametersReady event!"
					);
				}
			},
			e => {
				log_error!(self.logger, "Received unexpected liquidity event: {:?}", e);
			},
		}
	}

	pub(crate) async fn lsps2_receive_to_jit_channel(
		&self, amount_msat: u64, description: &str, expiry_secs: u32,
		max_total_lsp_fee_limit_msat: Option<u64>,
	) -> Result<(Bolt11Invoice, u64), Error> {
		let fee_response = self.lsps2_request_opening_fee_params().await?;

		let (min_total_fee_msat, min_opening_params) = fee_response
			.opening_fee_params_menu
			.into_iter()
			.filter_map(|params| {
				if amount_msat < params.min_payment_size_msat
					|| amount_msat > params.max_payment_size_msat
				{
					log_debug!(self.logger,
						"Skipping LSP-offered JIT parameters as the payment of {}msat doesn't meet LSP limits (min: {}msat, max: {}msat)",
						amount_msat,
						params.min_payment_size_msat,
						params.max_payment_size_msat
					);
					None
				} else {
					compute_opening_fee(amount_msat, params.min_fee_msat, params.proportional as u64)
						.map(|fee| (fee, params))
				}
			})
			.min_by_key(|p| p.0)
			.ok_or_else(|| {
				log_error!(self.logger, "Failed to handle response from liquidity service",);
				Error::LiquidityRequestFailed
			})?;

		if let Some(max_total_lsp_fee_limit_msat) = max_total_lsp_fee_limit_msat {
			if min_total_fee_msat > max_total_lsp_fee_limit_msat {
				log_error!(self.logger,
					"Failed to request inbound JIT channel as LSP's requested total opening fee of {}msat exceeds our fee limit of {}msat",
					min_total_fee_msat, max_total_lsp_fee_limit_msat
				);
				return Err(Error::LiquidityFeeTooHigh);
			}
		}

		log_debug!(
			self.logger,
			"Choosing cheapest liquidity offer, will pay {}msat in total LSP fees",
			min_total_fee_msat
		);

		let buy_response =
			self.lsps2_send_buy_request(Some(amount_msat), min_opening_params).await?;
		let invoice = self.lsps2_create_jit_invoice(
			buy_response,
			Some(amount_msat),
			description,
			expiry_secs,
		)?;

		log_info!(self.logger, "JIT-channel invoice created: {}", invoice);
		Ok((invoice, min_total_fee_msat))
	}

	pub(crate) async fn lsps2_receive_variable_amount_to_jit_channel(
		&self, description: &str, expiry_secs: u32,
		max_proportional_lsp_fee_limit_ppm_msat: Option<u64>,
	) -> Result<(Bolt11Invoice, u64), Error> {
		let fee_response = self.lsps2_request_opening_fee_params().await?;

		let (min_prop_fee_ppm_msat, min_opening_params) = fee_response
			.opening_fee_params_menu
			.into_iter()
			.map(|params| (params.proportional as u64, params))
			.min_by_key(|p| p.0)
			.ok_or_else(|| {
				log_error!(self.logger, "Failed to handle response from liquidity service",);
				Error::LiquidityRequestFailed
			})?;

		if let Some(max_proportional_lsp_fee_limit_ppm_msat) =
			max_proportional_lsp_fee_limit_ppm_msat
		{
			if min_prop_fee_ppm_msat > max_proportional_lsp_fee_limit_ppm_msat {
				log_error!(self.logger,
					"Failed to request inbound JIT channel as LSP's requested proportional opening fee of {} ppm msat exceeds our fee limit of {} ppm msat",
					min_prop_fee_ppm_msat,
					max_proportional_lsp_fee_limit_ppm_msat
				);
				return Err(Error::LiquidityFeeTooHigh);
			}
		}

		log_debug!(
			self.logger,
			"Choosing cheapest liquidity offer, will pay {}ppm msat in proportional LSP fees",
			min_prop_fee_ppm_msat
		);

		let buy_response = self.lsps2_send_buy_request(None, min_opening_params).await?;
		let invoice =
			self.lsps2_create_jit_invoice(buy_response, None, description, expiry_secs)?;

		log_info!(self.logger, "JIT-channel invoice created: {}", invoice);
		Ok((invoice, min_prop_fee_ppm_msat))
	}

	async fn lsps2_request_opening_fee_params(&self) -> Result<LSPS2FeeResponse, Error> {
		let lsps2_service = self.lsps2_service.as_ref().ok_or(Error::LiquiditySourceUnavailable)?;

		let client_handler = self.liquidity_manager.lsps2_client_handler().ok_or_else(|| {
			log_error!(self.logger, "Liquidity client was not configured.",);
			Error::LiquiditySourceUnavailable
		})?;

		let (fee_request_sender, fee_request_receiver) = oneshot::channel();
		{
			let mut pending_fee_requests_lock = lsps2_service.pending_fee_requests.lock().unwrap();
			let request_id = client_handler
				.request_opening_params(lsps2_service.node_id, lsps2_service.token.clone());
			pending_fee_requests_lock.insert(request_id, fee_request_sender);
		}

		tokio::time::timeout(
			Duration::from_secs(LIQUIDITY_REQUEST_TIMEOUT_SECS),
			fee_request_receiver,
		)
		.await
		.map_err(|e| {
			log_error!(self.logger, "Liquidity request timed out: {}", e);
			Error::LiquidityRequestFailed
		})?
		.map_err(|e| {
			log_error!(self.logger, "Failed to handle response from liquidity service: {}", e);
			Error::LiquidityRequestFailed
		})
	}

	async fn lsps2_send_buy_request(
		&self, amount_msat: Option<u64>, opening_fee_params: OpeningFeeParams,
	) -> Result<LSPS2BuyResponse, Error> {
		let lsps2_service = self.lsps2_service.as_ref().ok_or(Error::LiquiditySourceUnavailable)?;

		let client_handler = self.liquidity_manager.lsps2_client_handler().ok_or_else(|| {
			log_error!(self.logger, "Liquidity client was not configured.",);
			Error::LiquiditySourceUnavailable
		})?;

		let (buy_request_sender, buy_request_receiver) = oneshot::channel();
		{
			let mut pending_buy_requests_lock = lsps2_service.pending_buy_requests.lock().unwrap();
			let request_id = client_handler
				.select_opening_params(lsps2_service.node_id, amount_msat, opening_fee_params)
				.map_err(|e| {
					log_error!(
						self.logger,
						"Failed to send buy request to liquidity service: {:?}",
						e
					);
					Error::LiquidityRequestFailed
				})?;
			pending_buy_requests_lock.insert(request_id, buy_request_sender);
		}

		let buy_response = tokio::time::timeout(
			Duration::from_secs(LIQUIDITY_REQUEST_TIMEOUT_SECS),
			buy_request_receiver,
		)
		.await
		.map_err(|e| {
			log_error!(self.logger, "Liquidity request timed out: {}", e);
			Error::LiquidityRequestFailed
		})?
		.map_err(|e| {
			log_error!(self.logger, "Failed to handle response from liquidity service: {:?}", e);
			Error::LiquidityRequestFailed
		})?;

		Ok(buy_response)
	}

	fn lsps2_create_jit_invoice(
		&self, buy_response: LSPS2BuyResponse, amount_msat: Option<u64>, description: &str,
		expiry_secs: u32,
	) -> Result<Bolt11Invoice, Error> {
		let lsps2_service = self.lsps2_service.as_ref().ok_or(Error::LiquiditySourceUnavailable)?;

		// LSPS2 requires min_final_cltv_expiry_delta to be at least 2 more than usual.
		let min_final_cltv_expiry_delta = MIN_FINAL_CLTV_EXPIRY_DELTA + 2;
		let (payment_hash, payment_secret) = self
			.channel_manager
			.create_inbound_payment(None, expiry_secs, Some(min_final_cltv_expiry_delta))
			.map_err(|e| {
				log_error!(self.logger, "Failed to register inbound payment: {:?}", e);
				Error::InvoiceCreationFailed
			})?;

		let route_hint = RouteHint(vec![RouteHintHop {
			src_node_id: lsps2_service.node_id,
			short_channel_id: buy_response.intercept_scid,
			fees: RoutingFees { base_msat: 0, proportional_millionths: 0 },
			cltv_expiry_delta: buy_response.cltv_expiry_delta as u16,
			htlc_minimum_msat: None,
			htlc_maximum_msat: None,
		}]);

		let payment_hash = sha256::Hash::from_slice(&payment_hash.0).map_err(|e| {
			log_error!(self.logger, "Invalid payment hash: {:?}", e);
			Error::InvoiceCreationFailed
		})?;

		let currency = self.config.network.into();
		let mut invoice_builder = InvoiceBuilder::new(currency)
			.description(description.to_string())
			.payment_hash(payment_hash)
			.payment_secret(payment_secret)
			.current_timestamp()
			.min_final_cltv_expiry_delta(min_final_cltv_expiry_delta.into())
			.expiry_time(Duration::from_secs(expiry_secs.into()))
			.private_route(route_hint);

		if let Some(amount_msat) = amount_msat {
			invoice_builder = invoice_builder.amount_milli_satoshis(amount_msat).basic_mpp();
		}

		invoice_builder
			.build_signed(|hash| {
				Secp256k1::new()
					.sign_ecdsa_recoverable(hash, &self.keys_manager.get_node_secret_key())
			})
			.map_err(|e| {
				log_error!(self.logger, "Failed to build and sign invoice: {}", e);
				Error::InvoiceCreationFailed
			})
	}
}

#[derive(Debug, Clone)]
pub(crate) struct LSPS2FeeResponse {
	opening_fee_params_menu: Vec<OpeningFeeParams>,
}

#[derive(Debug, Clone)]
pub(crate) struct LSPS2BuyResponse {
	intercept_scid: u64,
	cltv_expiry_delta: u32,
}
