# mosh-rs 0.1.6

This maintenance release keeps bounded diagnostics in persistent endpoint state
directories. Client and server captures survive session exit, retain the latest
32 sessions per role, and include the server stderr sidecar for later disconnect
investigation.

Install version 0.1.6 on both endpoints. The client and server must use the same
version to establish a session.
