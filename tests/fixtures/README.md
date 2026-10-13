# FIX sample logs

Inputs for the tests: FIX 4.2 (SOH-delimited), 4.4 and 5.0SP2 messages, repeating groups,
and a set of deliberately malformed lines — wrong BodyLength, wrong CheckSum,
missing either, an unknown BeginString, a fragment, duplicate tags.

Every message is synthetic: invented counterparties, invented instruments, no
captured traffic. Much of it is deliberately wrong — of the 33 message lines, 18
carry a BodyLength or CheckSum that does not match their bytes, which is what
the diagnostics and repair tests are made of.

`tests/fixtures.rs` states the properties that hold over every one of these
lines; the rest of the suite uses individual files as inputs.
