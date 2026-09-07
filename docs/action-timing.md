# Action timing

Each device has minimum_action_latency_ms (0–5000 ms, default 30). This is a minimum interval from completion of the latest input dispatch to the next image observation. Existing devices use 30 ms when the setting is absent.

Actions in a batch have no implicit pauses, including typing. Only input resets the deadline; wait and screenshot do not. Elapsed time and explicit waits count toward the minimum. Later observations never restart the interval. Batches without observations return without waiting for the image deadline.

Use explicit wait actions between inputs when the target needs time to focus or process them. On a timing failure, increase the relevant wait by 50 ms until visually stable. After five verified successes, probe 25 ms lower, down to zero. If a lower value fails, restore the last stable value and probe again after five successes. Do not count dispatch acknowledgement as visual success or blindly replay uncertain input.

Example: click, wait(duration_ms=50), type. No per-action before/after parameters are supported. Timing knowledge belongs to the agent's current context, not persistent automatic calibration.

The interval measures local dispatch completion, not target processing or capture time. A fresh received frame is not proof that the target has completed the action.
