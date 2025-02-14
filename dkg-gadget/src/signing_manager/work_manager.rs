use crate::{
	async_protocols::remote::{AsyncProtocolRemote, ShutdownReason},
	debug_logger::DebugLogger,
	utils::SendFuture,
	worker::HasLatestHeader,
	NumberFor,
};
use dkg_primitives::{
	crypto::Public,
	types::{DKGError, SignedDKGMessage},
};
use dkg_runtime_primitives::{associated_block_id_acceptable, SessionId};
use parking_lot::RwLock;
use sp_api::BlockT;
use sp_arithmetic::traits::SaturatedConversion;
use std::{
	collections::{HashMap, HashSet, VecDeque},
	hash::{Hash, Hasher},
	pin::Pin,
	sync::Arc,
};
use sync_wrapper::SyncWrapper;

#[derive(Copy, Clone, PartialEq, Eq)]
pub enum PollMethod {
	Interval { millis: u64 },
	Manual,
}

#[derive(Clone)]
pub struct WorkManager<B: BlockT> {
	inner: Arc<RwLock<WorkManagerInner<B>>>,
	clock: Arc<dyn HasLatestHeader<B>>,
	// for now, use a hard-coded value for the number of tasks
	max_tasks: Arc<usize>,
	max_enqueued_tasks: Arc<usize>,
	logger: DebugLogger,
	poll_method: Arc<PollMethod>,
	to_handler: tokio::sync::mpsc::UnboundedSender<[u8; 32]>,
}

pub struct WorkManagerInner<B: BlockT> {
	pub active_tasks: HashSet<Job<B>>,
	pub enqueued_tasks: VecDeque<Job<B>>,
	// task hash => SSID => enqueued messages
	pub enqueued_messages: HashMap<[u8; 32], HashMap<u8, VecDeque<SignedDKGMessage<Public>>>>,
}

#[derive(Debug)]
pub struct JobMetadata {
	pub session_id: SessionId,
	pub is_stalled: bool,
	pub is_finished: bool,
	pub has_started: bool,
	pub is_active: bool,
}

impl<B: BlockT> WorkManager<B> {
	pub fn new(
		logger: DebugLogger,
		clock: impl HasLatestHeader<B>,
		max_tasks: usize,
		max_enqueued_tasks: usize,
		poll_method: PollMethod,
	) -> Self {
		let (to_handler, mut rx) = tokio::sync::mpsc::unbounded_channel();
		let this = Self {
			inner: Arc::new(RwLock::new(WorkManagerInner {
				active_tasks: HashSet::new(),
				enqueued_tasks: VecDeque::new(),
				enqueued_messages: HashMap::new(),
			})),
			clock: Arc::new(clock),
			max_tasks: Arc::new(max_tasks),
			max_enqueued_tasks: Arc::new(max_enqueued_tasks),
			logger,
			to_handler,
			poll_method: Arc::new(poll_method),
		};

		if let PollMethod::Interval { millis } = poll_method {
			let this_worker = this.clone();
			let handler = async move {
				let job_receiver_worker = this_worker.clone();
				let logger = job_receiver_worker.logger.clone();

				let job_receiver = async move {
					while let Some(task_hash) = rx.recv().await {
						job_receiver_worker
							.logger
							.info(format!("[worker] Received job {task_hash:?}",));
						job_receiver_worker.poll();
					}
				};

				let periodic_poller = async move {
					let mut interval =
						tokio::time::interval(std::time::Duration::from_millis(millis));
					loop {
						interval.tick().await;
						this_worker.poll();
					}
				};

				tokio::select! {
					_ = job_receiver => {
						logger.error("[worker] job_receiver exited");
					},
					_ = periodic_poller => {
						logger.error("[worker] periodic_poller exited");
					}
				}
			};

			tokio::task::spawn(handler);
		}

		this
	}

	/// Pushes the task, but does not necessarily start it
	pub fn push_task(
		&self,
		task_hash: [u8; 32],
		force_start: bool,
		mut handle: AsyncProtocolRemote<NumberFor<B>>,
		task: Pin<Box<dyn SendFuture<'static, ()>>>,
	) -> Result<(), DKGError> {
		let mut lock = self.inner.write();
		// set as primary, that way on drop, the async protocol ends
		handle.set_as_primary();
		let job = Job {
			task: Arc::new(RwLock::new(Some(task.into()))),
			handle,
			task_hash,
			logger: self.logger.clone(),
		};

		if force_start {
			// This job has priority over the max_tasks limit
			self.logger
				.debug(format!("[FORCE START] Force starting task {}", hex::encode(task_hash)));
			self.start_job_unconditional(job, &mut *lock);
			return Ok(())
		}

		lock.enqueued_tasks.push_back(job);

		if *self.poll_method != PollMethod::Manual {
			self.to_handler.send(task_hash).map_err(|_| DKGError::GenericError {
				reason: "Failed to send job to worker".to_string(),
			})
		} else {
			Ok(())
		}
	}

	pub fn can_submit_more_tasks(&self) -> bool {
		let lock = self.inner.read();
		lock.enqueued_tasks.len() < *self.max_enqueued_tasks
	}

	// Only relevant for keygen
	pub fn get_active_sessions_metadata(&self, now: NumberFor<B>) -> Vec<JobMetadata> {
		self.inner.read().active_tasks.iter().map(|r| r.metadata(now)).collect()
	}

	// This will shutdown and drop all tasks and enqueued messages
	pub fn force_shutdown_all(&self) {
		let mut lock = self.inner.write();
		lock.active_tasks.clear();
		lock.enqueued_tasks.clear();
		lock.enqueued_messages.clear();
	}

	pub fn poll(&self) {
		// Go through each task and see if it's done
		let now = self.clock.get_latest_block_number();
		let mut lock = self.inner.write();
		let cur_count = lock.active_tasks.len();
		lock.active_tasks.retain(|job| {
			let is_stalled = job.handle.signing_has_stalled(now);
			if is_stalled {
				// If stalled, lets log the start and now blocks for logging purposes
				self.logger.info(format!(
					"[worker] Job {:?} | Started at {:?} | Now {:?} | is stalled, shutting down",
					hex::encode(job.task_hash),
					job.handle.started_at,
					now
				));

				// The task is stalled, lets be pedantic and shutdown
				let _ = job.handle.shutdown(ShutdownReason::Stalled);
				// Return false so that the proposals are released from the currently signing
				// proposals
				return false
			}

			let is_done = job.handle.is_done();

			!is_done
		});

		let new_count = lock.active_tasks.len();
		if cur_count != new_count {
			self.logger.info(format!("[worker] {} jobs dropped", cur_count - new_count));
		}

		// Now, check to see if there is room to start a new task
		let tasks_to_start = self.max_tasks.saturating_sub(lock.active_tasks.len());
		for _ in 0..tasks_to_start {
			if let Some(job) = lock.enqueued_tasks.pop_front() {
				self.start_job_unconditional(job, &mut *lock);
			} else {
				break
			}
		}

		// Next, remove any outdated enqueued messages to prevent RAM bloat
		let mut to_remove = vec![];
		for (hash, queue) in lock.enqueued_messages.iter_mut() {
			for (ssid, queue) in queue.iter_mut() {
				let before = queue.len();
				// Only keep the messages that are not outdated
				queue.retain(|msg| {
					associated_block_id_acceptable(
						now.saturated_into(),
						msg.msg.associated_block_id,
					)
				});
				let after = queue.len();

				if before != after {
					self.logger.info(format!(
						"[worker] Removed {} outdated enqueued messages from the queue for {:?}",
						before - after,
						hex::encode(*hash)
					));
				}

				if queue.is_empty() {
					to_remove.push((*hash, *ssid));
				}
			}
		}

		// Next, to prevent the existence of piling-up empty *inner* queues, remove them
		for (hash, ssid) in to_remove {
			lock.enqueued_messages
				.get_mut(&hash)
				.expect("Should be available")
				.remove(&ssid);
		}

		// Finally, remove any empty outer maps
		lock.enqueued_messages.retain(|_, v| !v.is_empty());
	}

	fn start_job_unconditional(&self, job: Job<B>, lock: &mut WorkManagerInner<B>) {
		self.logger
			.info(format!("[worker] Starting job {:?}", hex::encode(job.task_hash)));
		if let Err(err) = job.handle.start() {
			self.logger
				.error(format!("Failed to start job {:?}: {err:?}", hex::encode(job.task_hash)));
		} else {
			// deliver all the enqueued messages to the protocol now
			if let Some(mut enqueued_messages_map) = lock.enqueued_messages.remove(&job.task_hash) {
				let job_ssid = job.handle.ssid;
				if let Some(mut enqueued_messages) = enqueued_messages_map.remove(&job_ssid) {
					self.logger.info(format!(
						"Will now deliver {} enqueued message(s) to the async protocol for {:?}",
						enqueued_messages.len(),
						hex::encode(job.task_hash)
					));

					while let Some(message) = enqueued_messages.pop_front() {
						if should_deliver(&job, &message, job.task_hash) {
							if let Err(err) = job.handle.deliver_message(message) {
								self.logger.error(format!(
									"Unable to deliver message for job {:?}: {err:?}",
									hex::encode(job.task_hash)
								));
							}
						} else {
							self.logger.warn("Will not deliver enqueued message to async protocol since the message is no longer acceptable")
						}
					}
				}

				// If there are any other messages for other SSIDs, put them back in the map
				if !enqueued_messages_map.is_empty() {
					lock.enqueued_messages.insert(job.task_hash, enqueued_messages_map);
				}
			}
		}
		let task = job.task.clone();
		// Put the job inside here, that way the drop code does not get called right away,
		// killing the process
		lock.active_tasks.insert(job);
		// run the task
		let task = async move {
			let task = task.write().take().expect("Should not happen");
			task.into_inner().await
		};

		// Spawn the task. When it finishes, it will clean itself up
		tokio::task::spawn(task);
	}

	pub fn job_exists(&self, job: &[u8; 32]) -> bool {
		let lock = self.inner.read();
		lock.active_tasks.contains(job) || lock.enqueued_tasks.iter().any(|j| &j.task_hash == job)
	}

	pub fn deliver_message(&self, msg: SignedDKGMessage<Public>, message_task_hash: [u8; 32]) {
		self.logger.debug(format!(
			"Delivered message is intended for session_id = {}",
			msg.msg.session_id
		));
		let mut lock = self.inner.write();

		// check the enqueued
		for task in lock.enqueued_tasks.iter() {
			if should_deliver(task, &msg, message_task_hash) {
				self.logger.debug(format!(
					"Message is for this ENQUEUED signing execution in session: {}",
					task.handle.session_id
				));
				if let Err(_err) = task.handle.deliver_message(msg) {
					self.logger.warn("Failed to deliver message to signing task");
				}

				return
			}
		}

		// check the currently signing
		for task in lock.active_tasks.iter() {
			if should_deliver(task, &msg, message_task_hash) {
				self.logger.debug(format!(
					"Message is for this signing CURRENT execution in session: {}",
					task.handle.session_id
				));
				if let Err(_err) = task.handle.deliver_message(msg) {
					self.logger.warn("Failed to deliver message to signing task");
				}

				return
			}
		}

		// if the protocol is neither started nor enqueued, then, this message may be for a future
		// async protocol. Store the message
		let current_running_session_ids: Vec<SessionId> =
			lock.active_tasks.iter().map(|job| job.handle.session_id).collect();
		let enqueued_session_ids: Vec<SessionId> =
			lock.enqueued_tasks.iter().map(|job| job.handle.session_id).collect();
		self.logger
			.info(format!("Enqueuing message for {:?} | current_running_session_ids: {current_running_session_ids:?} | enqueued_session_ids: {enqueued_session_ids:?}", hex::encode(message_task_hash)));
		lock.enqueued_messages
			.entry(message_task_hash)
			.or_default()
			.entry(msg.msg.ssid)
			.or_default()
			.push_back(msg)
	}
}

pub struct Job<B: BlockT> {
	// wrap in an arc to get the strong count for this job
	task_hash: [u8; 32],
	logger: DebugLogger,
	handle: AsyncProtocolRemote<NumberFor<B>>,
	task: Arc<RwLock<Option<SyncFuture<()>>>>,
}

impl<B: BlockT> Job<B> {
	fn metadata(&self, now: NumberFor<B>) -> JobMetadata {
		JobMetadata {
			session_id: self.handle.session_id,
			is_stalled: self.handle.keygen_has_stalled(now),
			is_finished: self.handle.is_keygen_finished(),
			has_started: self.handle.has_started(),
			is_active: self.handle.is_active(),
		}
	}
}

pub type SyncFuture<T> = SyncWrapper<Pin<Box<dyn SendFuture<'static, T>>>>;

impl<B: BlockT> std::borrow::Borrow<[u8; 32]> for Job<B> {
	fn borrow(&self) -> &[u8; 32] {
		&self.task_hash
	}
}

impl<B: BlockT> PartialEq for Job<B> {
	fn eq(&self, other: &Self) -> bool {
		self.task_hash == other.task_hash
	}
}

impl<B: BlockT> Eq for Job<B> {}

impl<B: BlockT> Hash for Job<B> {
	fn hash<H: Hasher>(&self, state: &mut H) {
		self.task_hash.hash(state);
	}
}

impl<B: BlockT> Drop for Job<B> {
	fn drop(&mut self) {
		self.logger.info(format!(
			"Will remove job {:?} from currently_signing_proposals",
			hex::encode(self.task_hash)
		));
		let _ = self.handle.shutdown(ShutdownReason::DropCode);
	}
}

fn should_deliver<B: BlockT>(
	task: &Job<B>,
	msg: &SignedDKGMessage<Public>,
	message_task_hash: [u8; 32],
) -> bool {
	task.handle.session_id == msg.msg.session_id &&
		task.task_hash == message_task_hash &&
		task.handle.ssid == msg.msg.ssid &&
		associated_block_id_acceptable(
			task.handle.associated_block_id,
			msg.msg.associated_block_id,
		)
}
