# Opus explicitly overrides CMAKE_MSVC_RUNTIME_LIBRARY unless this is enabled.
# Match the Rust release's +crt-static; setting /MT alone leaves mixed imports.
set(OPUS_STATIC_RUNTIME ON CACHE BOOL "Match the static Rust MSVC runtime" FORCE)
