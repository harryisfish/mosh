# mosh-rs 0.1.5

This maintenance release hardens reconnect recovery after suspension and network
flapping. It discards stale liveness probes, bounds DNS resolution during retry,
and applies capped backoff to repeated reconnect attempts.

Install version 0.1.5 on both endpoints. The client and server must use the same
version to establish a session.
