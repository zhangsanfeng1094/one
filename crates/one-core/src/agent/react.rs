use super::*;

type ParallelToolJob = (
    usize,
    ToolCall,
    ToolCall,
    Option<TraceGateDecision>,
    Arc<dyn Tool>,
);

/// Slot for concurrent tool execution (gate already applied).
enum ToolSlot {
    Pending {
        original: ToolCall,
        effective: ToolCall,
        gate: Option<TraceGateDecision>,
        tool: Arc<dyn Tool>,
    },
    Done {
        original: ToolCall,
        output: ToolOutput,
        is_error: bool,
        gate: Option<TraceGateDecision>,
        duration_ms: u64,
        /// Whether [`AgentEvent::ToolExecutionEnd`] / trace ToolEnd already fired.
        /// UI emits as soon as the tool finishes; agent ToolResult is recorded later
        /// in original call order.
        ui_emitted: bool,
    },
}

/// Drops once-use path tokens even if observational `after_tool` hangs or is skipped.
struct PermissionLeaseGuard {
    gate: Option<Arc<dyn ToolGate>>,
    call: ToolCall,
}

impl PermissionLeaseGuard {
    fn new(gate: Option<Arc<dyn ToolGate>>, call: ToolCall) -> Self {
        Self { gate, call }
    }
}

impl Drop for PermissionLeaseGuard {
    fn drop(&mut self) {
        if let Some(gate) = &self.gate {
            gate.release_permission_lease(&self.call);
        }
    }
}

pub(super) enum ParkOutcome {
    Satisfied,
    NewInput,
    Aborted,
}

/// Park ReAct loop waiting for background tasks in `interest` to reach terminal state.
///
/// Reliable pattern:
/// 1. Subscribe to input/abort waker FIRST (calls `OwnedNotified::enable()`).
/// 2. Check if already aborted, new input arrived, or satisfied. If so, return immediately.
/// 3. For remaining non-terminal IDs in interest, subscribe to task waiters.
/// 4. Recheck interest, abort, and new input under subscribed context.
/// 5. Await `futures::future::select_all` of subscribed waiters, racing against input/abort waiter.
/// 6. When awakened, loop back and recheck. No sleep/polling.
///
/// Emits `WaitParkStart` when actually parking (state leaves thinking/running-tools)
/// and `WaitParkEnd` on every exit path so listeners never see a stale waiting state.
pub(super) async fn park_wait_interest(agent: &Agent, interest: &mut WaitInterest) -> ParkOutcome {
    let mut parked = false;
    let outcome = park_wait_interest_inner(agent, interest, &mut parked).await;
    if parked {
        agent.emit(AgentEvent::WaitParkEnd);
    }
    outcome
}

async fn park_wait_interest_inner(
    agent: &Agent,
    interest: &mut WaitInterest,
    parked: &mut bool,
) -> ParkOutcome {
    loop {
        // Subscribe to input/abort waker before checking state to eliminate lost wakeup race
        let input_waiter = crate::RegisteredWaiter::new(agent.input_waker_handle());

        if agent.is_aborted() {
            return ParkOutcome::Aborted;
        }
        if agent.has_queued_messages() {
            return ParkOutcome::NewInput;
        }
        if interest.is_satisfied() {
            return ParkOutcome::Satisfied;
        }

        // Identify non-terminal IDs
        let non_terminal: Vec<String> = interest
            .ids
            .iter()
            .filter(|id| !interest.waiter.is_terminal(id))
            .cloned()
            .collect();

        if non_terminal.is_empty() {
            return ParkOutcome::Satisfied;
        }

        // Subscribe to all non-terminal targets first
        let subs: Vec<_> = non_terminal
            .iter()
            .map(|id| interest.waiter.subscribe_terminal(id))
            .collect();

        // Recheck under subscribed context
        if interest.is_satisfied() {
            return ParkOutcome::Satisfied;
        }
        if agent.is_aborted() {
            return ParkOutcome::Aborted;
        }
        if agent.has_queued_messages() {
            return ParkOutcome::NewInput;
        }

        // First time we actually block: notify listeners the loop is waiting.
        if !*parked {
            *parked = true;
            agent.emit(AgentEvent::WaitParkStart {
                mode: interest.mode,
                ids: interest.ids.clone(),
            });
        }

        // Race between tasks finishing, or input/abort waker firing.
        // Purely event-driven; no polling loop.
        tokio::select! {
            _ = input_waiter => {
                if agent.is_aborted() {
                    return ParkOutcome::Aborted;
                }
                if agent.has_queued_messages() {
                    return ParkOutcome::NewInput;
                }
            }
            _ = async {
                futures::future::select_all(subs).await;
            } => {
                if interest.is_satisfied() {
                    return ParkOutcome::Satisfied;
                }
            }
        }
    }
}

impl Agent {
    /// Execute pending slots. UI/trace `ToolEnd` fires as each tool finishes;
    /// agent `ToolResult` messages are recorded in original call order at the end.
    /// Consecutive read-only tools run concurrently; mutating tools run serially.
    async fn execute_slots(
        &mut self,
        slots: &mut Vec<ToolSlot>,
        turn: usize,
        run_id: &str,
        tool_results: &mut Vec<AgentMessage>,
    ) {
        self.execute_slots_react(slots, turn, run_id).await;

        // Record ToolResult messages in original tool-call order (providers match by id).
        for slot in std::mem::take(slots) {
            match slot {
                ToolSlot::Done {
                    original,
                    output,
                    is_error,
                    gate,
                    duration_ms,
                    ui_emitted,
                } => {
                    let execution = ToolExecutionResult {
                        output,
                        is_error,
                        gate_decision: gate,
                        duration_ms,
                    };
                    // Safety net: if emit was skipped, still fire UI end before recording.
                    if !ui_emitted {
                        self.emit_tool_end(&original, turn, run_id, &execution);
                    }
                    self.record_tool_result(&original, execution, tool_results);
                }
                ToolSlot::Pending {
                    original,
                    effective,
                    ..
                } => {
                    if let Some(g) = &self.tool_gate {
                        g.release_permission_lease(&effective);
                    }
                    self.finish_tool_result(
                        &original,
                        turn,
                        run_id,
                        ToolExecutionResult {
                            output: ToolOutput::text("internal error: tool not executed"),
                            is_error: true,
                            gate_decision: None,
                            duration_ms: 0,
                        },
                        tool_results,
                    );
                }
            }
        }
    }

    async fn execute_slots_react(&mut self, slots: &mut [ToolSlot], turn: usize, run_id: &str) {
        let n = slots.len();
        let mut i = 0;
        while i < n {
            // Already finished (e.g. gate deny) — notify UI now, record later.
            if matches!(&slots[i], ToolSlot::Done { .. }) {
                self.emit_slot_tool_end(slots, i, turn, run_id);
                i += 1;
                continue;
            }

            let side_effect = match &slots[i] {
                ToolSlot::Pending { original, .. } => !is_parallel_safe_tool(&original.name),
                ToolSlot::Done { .. } => false,
            };

            if side_effect {
                // UI ToolEnd before after_tool hooks (hooks must not delay the transcript).
                self.run_pending_at(slots, i, turn, run_id).await;
                i += 1;
                continue;
            }

            // Gather consecutive parallel-safe pending indices until a side-effecting pending.
            let mut batch: Vec<usize> = Vec::new();
            let mut k = i;
            while k < n {
                match &slots[k] {
                    ToolSlot::Pending { original, .. } if is_parallel_safe_tool(&original.name) => {
                        batch.push(k);
                        k += 1;
                    }
                    ToolSlot::Pending { .. } => break, // write/bash/MCP — stop before it
                    ToolSlot::Done { .. } => {
                        // Denial sitting between reads: notify UI now.
                        self.emit_slot_tool_end(slots, k, turn, run_id);
                        k += 1;
                    }
                }
            }

            if batch.is_empty() {
                // Should not happen (i was Pending parallel-safe).
                i += 1;
                continue;
            }

            self.run_pending_batch(slots, &batch, turn, run_id).await;
            i = k;
        }
    }

    /// Fire UI/trace ToolEnd for a Done slot (no-op if already emitted or still Pending).
    fn emit_slot_tool_end(
        &mut self,
        slots: &mut [ToolSlot],
        index: usize,
        turn: usize,
        run_id: &str,
    ) {
        let Some(slot) = slots.get_mut(index) else {
            return;
        };
        match slot {
            ToolSlot::Done {
                original,
                output,
                is_error,
                gate,
                duration_ms,
                ui_emitted,
            } if !*ui_emitted => {
                let execution = ToolExecutionResult {
                    output: output.clone(),
                    is_error: *is_error,
                    gate_decision: gate.clone(),
                    duration_ms: *duration_ms,
                };
                let call = original.clone();
                *ui_emitted = true;
                self.emit_tool_end(&call, turn, run_id, &execution);
            }
            _ => {}
        }
    }

    /// Run one pending tool, emit UI `ToolExecutionEnd`, then run after_tool hooks.
    ///
    /// **Order is intentional:** the transcript must flip the tool row as soon as
    /// `tool.execute` returns. Extension / permission `after_tool` hooks are
    /// observational and must not delay (or hang) the parent UI — especially for
    /// long-running tools like foreground `task` whose child already finalized.
    async fn run_pending_at(
        &mut self,
        slots: &mut [ToolSlot],
        index: usize,
        turn: usize,
        run_id: &str,
    ) {
        let (original, effective, gate, tool) = match &slots[index] {
            ToolSlot::Pending {
                original,
                effective,
                gate,
                tool,
            } => (
                original.clone(),
                effective.clone(),
                gate.clone(),
                Arc::clone(tool),
            ),
            ToolSlot::Done { .. } => return,
        };
        if self.is_aborted() {
            if let Some(g) = &self.tool_gate {
                g.release_permission_lease(&effective);
            }
            slots[index] = ToolSlot::Done {
                original,
                output: ToolOutput::text("aborted before tool execution"),
                is_error: true,
                gate,
                duration_ms: 0,
                ui_emitted: false,
            };
            self.emit_slot_tool_end(slots, index, turn, run_id);
            return;
        }
        let start = Instant::now();
        // Race tool work against Esc so long bash/network tools stop ~50ms after abort
        // (bash uses kill_on_drop; dropping the future cancels the child).
        let lease = PermissionLeaseGuard::new(self.tool_gate.clone(), effective.clone());
        let res =
            match crate::streaming::race_abort(tool.execute(&effective), Some(&self.abort_flag))
                .await
            {
                Ok(res) => res,
                Err(()) => Err(OneError::Aborted),
            };
        drop(lease);
        let duration_ms = start.elapsed().as_millis() as u64;
        let (output, is_error) = match res {
            Ok(output) => {
                let failed = tool_output_indicates_error(&original.name, &output);
                (output, failed)
            }
            Err(OneError::Aborted) => (ToolOutput::text("aborted"), true),
            Err(err) => (ToolOutput::text(err.to_string()), true),
        };
        // Capture hook inputs before moving into the slot.
        let hook_output = output.clone();
        slots[index] = ToolSlot::Done {
            original,
            output,
            is_error,
            gate,
            duration_ms,
            ui_emitted: false,
        };
        // Transcript first — parent row must not wait on after_tool.
        // Observational hooks are fire-and-forget with a hard cap so a stuck
        // extension/hook can never leave the parent agent parked on
        // "Thinking…" after a foreground `task` (or any tool) has already
        // returned. ToolExecutionEnd is already in the event queue for the UI.
        self.emit_slot_tool_end(slots, index, turn, run_id);
        if let Some(g) = self.tool_gate.clone() {
            let effective = effective.clone();
            tokio::spawn(async move {
                let fut = g.after_tool(&effective, &hook_output, is_error);
                let _ = tokio::time::timeout(Duration::from_secs(5), fut).await;
            });
        }
    }

    /// Run one independent operation group concurrently and report each result
    /// as it completes, while committing results in model call order.
    async fn run_pending_batch(
        &mut self,
        slots: &mut [ToolSlot],
        indices: &[usize],
        turn: usize,
        run_id: &str,
    ) {
        if indices.is_empty() {
            return;
        }
        if indices.len() == 1 {
            self.run_pending_at(slots, indices[0], turn, run_id).await;
            return;
        }

        let mut jobs: Vec<ParallelToolJob> = Vec::with_capacity(indices.len());
        for &i in indices {
            if let ToolSlot::Pending {
                original,
                effective,
                gate,
                tool,
            } = &slots[i]
            {
                jobs.push((
                    i,
                    original.clone(),
                    effective.clone(),
                    gate.clone(),
                    Arc::clone(tool),
                ));
            }
        }

        let abort = self.abort_flag.clone();
        let tool_gate = self.tool_gate.clone();
        let limit = self.config.max_parallel_readonly_tools.max(1);
        let semaphore = Arc::new(tokio::sync::Semaphore::new(limit));
        let mut futs = futures::stream::FuturesUnordered::new();
        for (i, _original, effective, _gate, tool) in &jobs {
            let tool = Arc::clone(tool);
            let effective = effective.clone();
            let abort = abort.clone();
            let tool_gate = tool_gate.clone();
            let semaphore = Arc::clone(&semaphore);
            let idx = *i;
            futs.push(async move {
                let permit = semaphore.acquire_owned().await.ok();
                let lease = PermissionLeaseGuard::new(tool_gate, effective.clone());
                let start = Instant::now();
                let res = if abort.load(Ordering::Relaxed) {
                    Err(OneError::Aborted)
                } else {
                    match crate::streaming::race_abort(
                        tool.execute(&effective),
                        Some(abort.as_ref()),
                    )
                    .await
                    {
                        Ok(res) => res,
                        Err(()) => Err(OneError::Aborted),
                    }
                };
                drop(lease);
                drop(permit);
                (idx, res, start.elapsed().as_millis() as u64)
            });
        }

        // Map slot index → job metadata for after_tool / Done construction.
        let mut by_index: std::collections::HashMap<
            usize,
            (ToolCall, ToolCall, Option<TraceGateDecision>),
        > = jobs
            .into_iter()
            .map(|(i, original, effective, gate, _)| (i, (original, effective, gate)))
            .collect();

        while let Some((i, res, duration_ms)) = futs.next().await {
            let Some((original, effective, gate)) = by_index.remove(&i) else {
                continue;
            };
            let (output, is_error) = match res {
                Ok(output) => {
                    let failed = tool_output_indicates_error(&original.name, &output);
                    (output, failed)
                }
                Err(OneError::Aborted) => (ToolOutput::text("aborted"), true),
                Err(err) => (ToolOutput::text(err.to_string()), true),
            };
            let hook_output = output.clone();
            slots[i] = ToolSlot::Done {
                original,
                output,
                is_error,
                gate,
                duration_ms,
                ui_emitted: false,
            };
            // UI flips this row to Done as soon as *this* tool finishes — not when
            // the whole parallel group drains, and not after after_tool hooks.
            // Fire-and-forget after_tool (same policy as run_pending_at).
            self.emit_slot_tool_end(slots, i, turn, run_id);
            if let Some(g) = self.tool_gate.clone() {
                let effective = effective.clone();
                tokio::spawn(async move {
                    let fut = g.after_tool(&effective, &hook_output, is_error);
                    let _ = tokio::time::timeout(Duration::from_secs(5), fut).await;
                });
            }
        }
    }
}

impl Agent {
    /// ReAct gates a complete tool batch, then executes read-only groups and
    /// side-effecting calls in their original order.
    pub(super) async fn run_tool_batch(
        &mut self,
        tool_calls: &[ToolCall],
        turn: usize,
        run_id: &str,
        tool_results: &mut Vec<AgentMessage>,
    ) -> ToolBatchOutcome {
        let mut slots: Vec<ToolSlot> = Vec::with_capacity(tool_calls.len());

        for (i, call) in tool_calls.iter().enumerate() {
            if self.is_aborted() {
                self.execute_slots(&mut slots, turn, run_id, tool_results)
                    .await;
                for call in &tool_calls[i..] {
                    self.emit_synthetic_skip(
                        call,
                        turn,
                        run_id,
                        "aborted before tool execution",
                        tool_results,
                    );
                }
                return ToolBatchOutcome::Aborted;
            }
            if i > 0 && self.has_steering() {
                self.execute_slots(&mut slots, turn, run_id, tool_results)
                    .await;
                for call in &tool_calls[i..] {
                    self.emit_synthetic_skip(
                        call,
                        turn,
                        run_id,
                        "skipped: user steering message queued",
                        tool_results,
                    );
                }
                return ToolBatchOutcome::Continue;
            }

            let (args_bytes, preview) = args_preview(&call.arguments, self.preview_limit());
            self.record_trace(TraceEvent::ToolStart {
                ts_ms: now_ms(),
                run_id: run_id.to_string(),
                turn,
                call_id: call.id.clone(),
                name: call.name.clone(),
                args_bytes,
                args_preview: preview,
            });
            self.emit(AgentEvent::ToolExecutionStart {
                tool_call: call.clone(),
            });
            match self.gate_tool(call, run_id, turn).await {
                GateOutcome::Allow {
                    effective,
                    gate,
                    tool,
                } => slots.push(ToolSlot::Pending {
                    original: call.clone(),
                    effective,
                    gate,
                    tool,
                }),
                GateOutcome::Deny { message, gate } => slots.push(ToolSlot::Done {
                    original: call.clone(),
                    output: ToolOutput::text(message),
                    is_error: true,
                    gate,
                    duration_ms: 0,
                    ui_emitted: false,
                }),
            }
        }

        self.execute_slots(&mut slots, turn, run_id, tool_results)
            .await;
        if self.is_aborted() {
            ToolBatchOutcome::Aborted
        } else {
            ToolBatchOutcome::Continue
        }
    }
}
