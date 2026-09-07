# Browser frame recovery

Transient browser frame-fetch failures can occur while subsequent images continue to update. A failed fetch alone does not establish that the KVM device is offline.

Previously every rejected frame request immediately set the same error field used by input operations; the next successful poll cleared it. This produced flashing warnings for brief transport failures and could also hide genuine input errors.

The screen now keeps frame errors separate from operation errors, retains the last complete image, and retries normally. Transient fetch errors become a visible frame warning only when the last received frame is at least two seconds old (or no frame exists). The age includes the server-reported frame age. HTTP 401/403 errors remain immediately visible. Fetches have a five-second timeout and are aborted on unmount; a successful frame clears only frame errors. No control/input request is replayed by this change.

Validation: Vue typecheck and production build pass. `node scripts/ui-frame-recovery.mjs` injects failures into two simulator tiles, verifies no flashing during a short outage, warnings for sustained failures, retained images, automatic visual recovery, and operation errors remaining visible through successful polls. The test never sends hardware input or changes the user's saved layout. This fixes the UI reaction to transient failures; it does not claim to eliminate an unobserved network fault.
