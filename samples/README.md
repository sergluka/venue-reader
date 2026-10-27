# Synthetic FIX5 sample logs

All messages were constructed for this repository from public FIX definitions.
Counterparties, instruments, orders and other identifiers are invented and use
the `SYNTHETIC_` prefix. No captured traffic or venue-specific dictionaries are
included.

| File | Application release | Separator | Messages |
| --- | --- | --- | --- |
| [fix50.fix](fix50.fix) | FIX 5.0 | Pipe | 10 |
| [fix50sp1.fix](fix50sp1.fix) | FIX 5.0 SP1 | Pipe | 10 |
| [fix50sp2.fix](fix50sp2.fix) | FIX 5.0 SP2 | Pipe | 10 |
| [fix5-mixed.fix](fix5-mixed.fix) | Interleaved FIX 5.0, SP1 and SP2 | Pipe | 30 |
| [fix50sp2-soh.fix](fix50sp2-soh.fix) | FIX 5.0 SP2 | SOH (`0x01`) | 10 |

Each version contains Logon, NewOrderSingle, ExecutionReport,
MarketDataRequest, MarketDataSnapshotFullRefresh, MarketDataIncrementalRefresh,
BusinessMessageReject, TestRequest, Heartbeat and Logout examples. Market-data
messages include repeating groups. The mixed file interleaves three independent
synthetic sessions; these are illustrative messages, not a complete session
transcript or a venue certification fixture.

BeginString(8) is `FIXT.1.1` in every message. Every application message includes
ApplVerID(1128): `7` for FIX 5.0, `8` for SP1, or `9` for SP2. Administrative
messages omit application selectors. DefaultApplVerID(1137) appears in Logon as
protocol data; decoding subsequent lines does not depend on it. With default
settings, the bundled dictionaries suffice; no external files or modelines
are required.

BodyLength(9) and CheckSum(10) are calculated from wire bytes with SOH separators.
The pipe files replace SOH with `|` for readability; Venue Reader normalizes those
separators for integrity checks. The SOH file contains actual control bytes.
Comments and line-ending bytes are outside each framed message.

## Public sources

- [FIXT 1.1 Errata](https://www.fixtrading.org/wp-content/uploads/download-manager-files/FIX_Transport_1.1.pdf)
  defines transport framing and application-version identification.
- [FIX TagValue Encoding Standard](https://fixtrading.org/packages/fix-tagvalue-encoding/)
  describes tag-value encoding with SOH separators.
- The pinned QuickFIX/J dictionaries in [xml/standard](../xml/standard/), with
  public source revision and licenses recorded in [xml/README.md](../xml/README.md),
  supply message fields, enum values and repeating-group definitions.

Protocol references used for generation are archived locally under
`docs/fix-version-model/` and `docs/fix5-samples/` with provenance. The archived
references are not required to open these logs.
