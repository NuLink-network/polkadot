// Copyright 2021 Parity Technologies (UK) Ltd.
// This file is part of Polkadot.

// Polkadot is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// Polkadot is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with Polkadot.  If not, see <http://www.gnu.org/licenses/>.


use std::collections::HashMap;
use std::collections::HashSet;

use futures::Future;
use futures::FutureExt;
use futures::SinkExt;
use futures::channel::mpsc;
use futures::future::RemoteHandle;

use polkadot_node_network_protocol::{
	IfDisconnected,
	request_response::{
		OutgoingRequest, OutgoingResult, Recipient, Requests,
		v1::{DisputeRequest, DisputeResponse},
	}
};
use polkadot_node_subsystem_util::runtime::RuntimeInfo;
use polkadot_primitives::v1::{
	AuthorityDiscoveryId, CandidateHash, Hash, SessionIndex, ValidatorIndex,
};
use polkadot_subsystem::{
	SubsystemContext,
	messages::{AllMessages, NetworkBridgeMessage},
};

use super::error::{Fatal, Result};

use crate::LOG_TARGET;

/// Delivery status for a particular dispute.
///
/// Keeps track of all the validators that have to be reached for a dispute.
pub struct SendTask {
	/// The request we are supposed to get out to all parachain validators of the dispute's session
	/// and to all current authorities.
	request: DisputeRequest,

	/// The set of authorities we need to send our messages to. This set will change at session
	/// boundaries. It will always be at least the parachain validators of the session where the
	/// dispute happened and the authorities of the current sessions as determined by active heads.
	deliveries: HashMap<AuthorityDiscoveryId, DeliveryStatus>,

	/// Whether or not we have any tasks failed since the last refresh.
	has_failed_sends: bool,

	/// Sender to be cloned for tasks.
	tx: mpsc::Sender<FromSendingTask>,
}

/// Status of a particular vote/statement delivery to a particular validator.
enum DeliveryStatus {
	/// Request is still in flight.
	Pending(RemoteHandle<()>),
	/// Succeeded - no need to send request to this peer anymore.
	Succeeded,
}

/// Messages from tasks trying to get disputes delievered.
#[derive(Debug)]
pub enum FromSendingTask {
	/// Delivery of statements for given candidate finished for this authority.
	Finished(CandidateHash, AuthorityDiscoveryId, TaskResult),
}

#[derive(Debug)]
pub enum TaskResult {
	/// Task succeeded in getting the request to its peer.
	Succeeded,
	/// Task was not able to get the request out to its peer.
	///
	/// It should be retried in that case.
	Failed,
}

impl SendTask
{
	/// Initiates sending a dispute message to peers.
	pub async fn new<Context: SubsystemContext>(
		ctx: &mut Context,
		runtime: &mut RuntimeInfo,
		active_sessions: &HashMap<SessionIndex,Hash>,
		tx: mpsc::Sender<FromSendingTask>,
		request: DisputeRequest,
	) -> Result<Self> {
		let mut send_task = Self {
			request,
			deliveries: HashMap::new(),
			has_failed_sends: false,
			tx,
		};
		send_task.refresh_sends(
			ctx,
			runtime,
			active_sessions,
		).await?;
		Ok(send_task)
	}

	/// Make sure we are sending to all relevant authorities.
	///
	/// This function is called at construction and should also be called whenever a session change
	/// happens and on a regular basis to ensure we are retrying failed attempts.
	pub async fn refresh_sends<Context: SubsystemContext>(
		&mut self,
		ctx: &mut Context,
		runtime: &mut RuntimeInfo,
		active_sessions: &HashMap<SessionIndex, Hash>,
	) -> Result<()> {
		let new_authorities = self.get_relevant_validators(ctx, runtime, active_sessions).await?;

		let add_authorities = new_authorities
			.iter()
			.filter(|a| !self.deliveries.contains_key(a))
			.map(Clone::clone)
			.collect();

		// Get rid of dead/irrelevant tasks/statuses:
		self.deliveries.retain(|k, _| new_authorities.contains(k));

		// Start any new tasks that are needed:
		let new_statuses = send_requests(
			ctx,
			self.tx.clone(),
			add_authorities,
			self.request.clone(),
		).await?;

		self.deliveries.extend(new_statuses.into_iter());
		self.has_failed_sends = false;
		Ok(())
	}

	/// Whether or not any sends have failed since the last refreshed.
	pub fn has_failed_sends(&self) -> bool {
		self.has_failed_sends
	}

	/// Handle a finished response waiting task.
	pub fn on_finished_send(&mut self, authority: &AuthorityDiscoveryId, result: TaskResult) {
		match result {
			TaskResult::Failed => {
				tracing::warn!(
					target: LOG_TARGET,
					candidate = ?self.request.0.candidate_receipt.hash(),
					?authority,
					"Could not get our message out! If this keeps happening, then check chain whether the dispute made it there."
				);
				self.has_failed_sends = true;
				// Remove state, so we know what to try again:
				self.deliveries.remove(authority);
			}
			TaskResult::Succeeded => {
				let status = match self.deliveries.get_mut(&authority) {
					None => {
						// Can happen when a sending became irrelevant while the response was already
						// queued.
						tracing::debug!(
							target: LOG_TARGET,
							candidate = ?self.request.0.candidate_receipt.hash(),
							?authority,
							?result,
							"Received `FromSendingTask::Finished` for non existing task."
						);
						return
					}
					Some(status) => status,
				};
				// We are done here:
				*status = DeliveryStatus::Succeeded;
			}
		}
	}


	/// Determine all validators that should receive the given dispute requests.
	///
	/// This is all parachain validators of the session the candidate occurred and all authorities
	/// of all currently active sessions, determined by currently active heads.
	async fn get_relevant_validators<Context: SubsystemContext>(
		&self,
		ctx: &mut Context,
		runtime: &mut RuntimeInfo,
		active_sessions: &HashMap<SessionIndex, Hash>,
	) -> Result<HashSet<AuthorityDiscoveryId>> {
		let ref_head = self.request.0.candidate_receipt.descriptor.relay_parent;
		// Parachain validators:
		let info = runtime
			.get_session_info_by_index(ctx.sender(), ref_head, self.request.0.session_index)
			.await?;
		let session_info = &info.session_info;
		let validator_count = session_info.validators.len();
		let mut authorities: HashSet<_> = session_info
			.discovery_keys
			.iter()
			.take(validator_count)
			.enumerate()
			.filter(|(i, _)| Some(ValidatorIndex(*i as _)) != info.validator_info.our_index)
			.map(|(_, v)| v.clone())
			.collect();

		// Current authorities:
		for (session_index, head) in active_sessions.iter() {
			let info = runtime.get_session_info_by_index(ctx.sender(), *head, *session_index).await?;
			let session_info = &info.session_info;
			let new_set = session_info
				.discovery_keys
				.iter()
				.enumerate()
				.filter(|(i, _)| Some(ValidatorIndex(*i as _)) != info.validator_info.our_index)
				.map(|(_, v)| v.clone());
			authorities.extend(new_set);
		}
		Ok(authorities)
	}
}


/// Start sending of the given msg to all given authorities.
///
/// And spawn tasks for handling the response.
async fn send_requests<Context: SubsystemContext>(
	ctx: &mut Context,
	tx: mpsc::Sender<FromSendingTask>,
	receivers: Vec<AuthorityDiscoveryId>,
	req: DisputeRequest,
) -> Result<HashMap<AuthorityDiscoveryId, DeliveryStatus>> {
	let mut statuses = HashMap::with_capacity(receivers.len());
	let mut reqs = Vec::with_capacity(receivers.len());

	for receiver in receivers {
		let (outgoing, pending_response) = OutgoingRequest::new(
			Recipient::Authority(receiver.clone()),
			req.clone(),
		);

		reqs.push(Requests::DisputeSending(outgoing));

		let fut = wait_response_task(
			pending_response,
			req.0.candidate_receipt.hash(),
			receiver.clone(),
			tx.clone(),
		);

		let (remote, remote_handle) = fut.remote_handle();
		ctx.spawn("dispute-sender", remote.boxed())
			.map_err(Fatal::SpawnTask)?;
		statuses.insert(receiver, DeliveryStatus::Pending(remote_handle));
	}

	let msg = NetworkBridgeMessage::SendRequests(
		reqs,
		// We should be connected, but the hell - if not, try!
		IfDisconnected::TryConnect,
	);
	ctx.send_message(AllMessages::NetworkBridge(msg)).await;
	Ok(statuses)
}

/// Future to be spawned in a task for awaiting a response.
async fn wait_response_task(
	pending_response: impl Future<Output = OutgoingResult<DisputeResponse>>,
	candidate_hash: CandidateHash,
	receiver: AuthorityDiscoveryId,
	mut tx: mpsc::Sender<FromSendingTask>,
) {
	let result = pending_response.await;
	let msg = match result {
		Err(err) => {
			tracing::warn!(
				target: LOG_TARGET,
				%candidate_hash,
				%receiver,
				%err,
				"Error sending dispute statements to node."
			);
			FromSendingTask::Finished(candidate_hash, receiver, TaskResult::Failed)
		}
		Ok(DisputeResponse::Confirmed) => {
			tracing::trace!(
				target: LOG_TARGET,
				%candidate_hash,
				%receiver,
				"Sending dispute message succeeded"
			);
			FromSendingTask::Finished(candidate_hash, receiver, TaskResult::Succeeded)
		}
	};
	if let Err(err) = tx.feed(msg).await {
		tracing::debug!(
			target: LOG_TARGET,
			%err,
			"Failed to notify susystem about dispute sending result."
		);
	}
}
