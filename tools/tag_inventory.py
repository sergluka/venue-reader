#!/usr/bin/env python3
"""Writes docs/tags.md and docs/tags-inventory.md from xml/standard/.

Reads the pinned sources only: the QuickFIX/J dictionaries and the FIX Latest
projection. Nothing here is a judgement; every list is a fact about the files,
grouped so the places where the sources disagree stand out.

    tools/tag_inventory.py
"""

from __future__ import annotations

import textwrap
import xml.etree.ElementTree as ET
from collections import Counter, defaultdict
from dataclasses import dataclass, field
from functools import cached_property
from itertools import pairwise
from pathlib import Path

from orchestra_projection import extension_pack

ROOT = Path(__file__).resolve().parent.parent
XML = ROOT / "xml" / "standard"
DOCS = ROOT / "docs"

VERSIONS = [
    ("FIX.4.0", "FIX40.xml"),
    ("FIX.4.1", "FIX41.xml"),
    ("FIX.4.2", "FIX42.xml"),
    ("FIX.4.3", "FIX43.xml"),
    ("FIX.4.4", "FIX44.xml"),
    ("FIX.5.0", "FIX50.xml"),
    ("FIX.5.0SP1", "FIX50SP1.xml"),
    ("FIX.5.0SP2", "FIX50SP2.xml"),
]
SESSION = ("FIXT.1.1", "FIXT11.xml")
METADATA = "FIXLatest-metadata.xml"
ORDER = [version for version, _ in VERSIONS]
LAST = ORDER[-1]
# The application files that leave the session layer to FIXT11.xml.
APPLICATION_5X = ("FIX.5.0", "FIX.5.0SP1", "FIX.5.0SP2")
RANK = {version: i for i, version in enumerate(ORDER)}
# Orchestra pedigree values that are not bundled versions, in protocol order.
RANK.update({"FIX.2.7": -2, "FIX.3.0": -1, "FIX.Latest": len(ORDER)})
SHORT = {version: version.replace("FIX.", "").replace("SP", "sp") for version in ORDER}
SHORT[SESSION[0]] = "T1.1"

MSG_TYPE = 35
# BenchmarkCurveCurrency(220), whose own description dates it to FIX 4.3.
BENCHMARK_CURVE_CURRENCY = 220
USER_DEFINED_START = 5000
# Tag ranges by lower bound, as the FIX specification assigns them.
TAG_RANGES = [
    (0, "< 5000"),
    (5000, "5000–9999"),
    (10000, "10000–19999"),
    (20000, "20000–39999"),
    (40000, "40000–49999"),
    (50000, "≥ 50000"),
]
STANDARD_AGAIN = 40000
EP_BUCKET = 50
MISSING_VALUES_SHOWN = 400
DEPRECATED_SHOWN = 60


@dataclass
class QuickfixField:
    tag: int
    name: str
    type: str
    values: list[tuple[str, str]]  # (enum, description)


@dataclass
class QuickfixDict:
    version: str
    file: str
    fields: dict[int, QuickfixField]
    referenced: set[str]  # field names used by header/trailer/messages/components

    def unreferenced(self) -> list[int]:
        return sorted(
            tag for tag, entry in self.fields.items() if entry.name not in self.referenced
        )

    def listed(self, tag: int) -> set[str]:
        return {value for value, _ in self.fields[tag].values}


@dataclass
class LatestCode:
    value: str
    name: str
    added: str
    added_ep: int
    deprecated: str


@dataclass
class LatestField:
    tag: int
    name: str
    type: str
    added: str
    added_ep: int
    deprecated: str
    own_code_set: bool
    codes: list[LatestCode] = field(default_factory=list)


def read_quickfix(version: str, file: str) -> QuickfixDict:
    root = ET.parse(XML / file).getroot()
    fields: dict[int, QuickfixField] = {}
    for node in root.find("fields").findall("field"):
        tag = int(node.get("number"))
        values = [
            (value.get("enum"), value.get("description", "")) for value in node.findall("value")
        ]
        fields[tag] = QuickfixField(tag, node.get("name"), node.get("type", ""), values)
    referenced: set[str] = set()
    for section in ("header", "trailer", "messages", "components"):
        part = root.find(section)
        if part is None:
            continue
        for node in part.iter():
            if node.tag in ("field", "group") and node.get("name"):
                referenced.add(node.get("name"))
    return QuickfixDict(version, file, fields, referenced)


def read_latest() -> tuple[dict[int, LatestField], str]:
    root = ET.parse(XML / METADATA).getroot()
    fields: dict[int, LatestField] = {}
    for node in root.find("fields").findall("field"):
        codes = [
            LatestCode(
                code.get("value"),
                code.get("name", ""),
                code.get("added", ""),
                extension_pack(code.get("addedEP", "")),
                code.get("deprecated", ""),
            )
            for code in node.findall("code")
        ]
        tag = int(node.get("id"))
        fields[tag] = LatestField(
            tag,
            node.get("name"),
            node.get("type", ""),
            node.get("added", ""),
            extension_pack(node.get("addedEP", "")),
            node.get("deprecated", ""),
            node.get("ownCodeSet") == "true",
            codes,
        )
    return fields, root.get("source", "")


@dataclass
class Sources:
    """Everything read from xml/standard/, and the sets every section asks about."""

    quickfix: dict[str, QuickfixDict]
    session: QuickfixDict
    latest: dict[int, LatestField]
    latest_source: str
    checksums: dict[str, str]

    @classmethod
    def read(cls) -> Sources:
        latest, latest_source = read_latest()
        lines = (XML / "SHA256SUMS").read_text().splitlines()
        return cls(
            quickfix={version: read_quickfix(version, file) for version, file in VERSIONS},
            session=read_quickfix(*SESSION),
            latest=latest,
            latest_source=latest_source,
            checksums=dict(line.split()[::-1] for line in lines if line),
        )

    @cached_property
    def application(self) -> set[int]:
        """Tags in the per-version application files."""
        return set().union(*(entry.fields for entry in self.quickfix.values()))

    @cached_property
    def bundled(self) -> set[int]:
        """Tags in any QuickFIX/J file, session layer included."""
        return self.application | set(self.session.fields)

    @cached_property
    def present(self) -> dict[int, list[str]]:
        """The application versions that declare each tag, in order."""
        return {
            tag: [
                version
                for version in ORDER
                if version in self.quickfix and tag in self.quickfix[version].fields
            ]
            for tag in self.bundled
        }

    @cached_property
    def only_latest(self) -> list[int]:
        return sorted(set(self.latest) - self.bundled)

    @cached_property
    def only_bundled(self) -> list[int]:
        return sorted(self.bundled - set(self.latest))

    @cached_property
    def application_in_latest(self) -> list[int]:
        return sorted(self.application & set(self.latest))

    @cached_property
    def code_sets(self) -> dict[tuple, list[int]]:
        """FIX Latest fields grouped by identical code lists."""
        sets: dict[tuple, list[int]] = {}
        for entry in self.latest.values():
            if entry.codes:
                key = tuple(
                    (code.value, code.name, code.added, code.added_ep) for code in entry.codes
                )
                sets.setdefault(key, []).append(entry.tag)
        return sets

    def name(self, tag: int) -> str:
        """FIX Latest's name, else the first QuickFIX/J file's."""
        if tag in self.latest:
            return self.latest[tag].name
        return next(
            (entry.fields[tag].name for entry in self.quickfix.values() if tag in entry.fields),
            "?",
        )


class Document:
    """Markdown, collected in order."""

    def __init__(self) -> None:
        self.lines: list[str] = []

    def text(self, block: str) -> None:
        """A block of lines, dedented, then a blank line."""
        self.lines.extend(textwrap.dedent(block).strip("\n").split("\n"))
        self.lines.append("")

    def table(
        self,
        header: list[str],
        rows: list[list[object]],
        empty: str = "None.",
        limit: int | None = None,
    ) -> None:
        """A table, `empty` when there are no rows; past `limit` rows, a count of the rest."""
        shown = rows if limit is None else rows[:limit]
        self.text(markdown_table(header, shown) if shown else empty)
        if limit is not None and len(rows) > limit:
            self.text(f"… {len(rows) - limit} more.")

    def render(self) -> str:
        return "\n".join(self.lines) + "\n"


def markdown_table(header: list[str], rows: list[list[object]]) -> str:
    lines = ["| " + " | ".join(header) + " |", "|" + "|".join(" --- " for _ in header) + "|"]
    lines.extend("| " + " | ".join(str(cell) for cell in row) + " |" for row in rows)
    return "\n".join(lines)


def dating(item: LatestField | LatestCode) -> str:
    """`added` with its extension pack, `—` when undated."""
    pack = f" EP{item.added_ep}" if item.added_ep > 0 else ""
    return (item.added or "—") + pack


def label(tag: int, name: str) -> str:
    return f"{name}({tag})"


def short_list(versions: list[str]) -> str:
    return ", ".join(SHORT[version] for version in versions)


def runs(sequence: list[tuple[str, str]]) -> str:
    """`(version, value)` pairs as runs of equal values: `A (4.0, 4.1); B (4.2)`."""
    grouped: list[tuple[list[str], str]] = []
    for version, value in sequence:
        if grouped and grouped[-1][1] == value:
            grouped[-1][0].append(version)
        else:
            grouped.append(([version], value))
    return "; ".join(f"{value} ({short_list(versions)})" for versions, value in grouped)


def tag_range(tag: int) -> str:
    return next(name for start, name in reversed(TAG_RANGES) if tag >= start)


def has_gap(ranks: list[int]) -> bool:
    return any(later - earlier > 1 for earlier, later in pairwise(ranks))


def values_absent_from_latest(sources: Sources) -> list[tuple[int, str, dict[str, list[str]]]]:
    """(tag, value, versions by description) for values FIX Latest's set lacks."""
    result = []
    for tag in sources.application_in_latest:
        latest_values = {code.value for code in sources.latest[tag].codes}
        if not latest_values:
            continue
        descriptions = defaultdict(lambda: defaultdict(list))
        for version in sources.present[tag]:
            for value, description in sources.quickfix[version].fields[tag].values:
                if value not in latest_values:
                    descriptions[value][description].append(version)
        result.extend(
            (tag, value, dict(by_description)) for value, by_description in descriptions.items()
        )
    return result


# --- docs/tags.md, one function per section ---------------------------------


def write_sources(sources: Sources, doc: Document) -> None:
    doc.text("""
        # FIX tags: what the bundled sources say, and where they disagree
    """)
    doc.text("""
        Generated by `tools/tag_inventory.py` from `xml/standard/`; do not edit by
        hand. Every number below is computed from the pinned files, so it can be
        re-derived after a source update.
    """)
    doc.text("""
        The full per-tag matrix is in [`tags-inventory.md`](tags-inventory.md).
    """)
    doc.text("## Sources")
    doc.text("Two families, used for different things:")
    doc.text(f"""
        - **QuickFIX/J dictionaries** (`FIX40.xml` … `FIX50SP2.xml`, `FIXT11.xml`):
          one file per FIX version as QuickFIX/J ships it. They decide which tags,
          values, messages and groups a *version* has. Frozen at each release;
          extension packs published after FIX 5.0 SP2 are not in them.
        - **FIX Latest** (`FIXLatest-metadata.xml`, projected from Orchestra
          `{sources.latest_source}`): the current standard as one cumulative list. Every
          field and code carries its pedigree (`added`, `addedEP`, `deprecated`).
          It knows every extension pack but has no per-version structure and
          records no removals.
    """)
    files = [file for _, file in [*VERSIONS, SESSION]] + [METADATA]
    rows = [[file, sources.checksums.get(file, "")[:12] + "…"] for file in files]
    doc.table(["File", "SHA-256 (prefix)"], rows)


def write_counts(sources: Sources, doc: Document) -> None:
    doc.text("## 1. Counts")

    def row(name: str, entry: QuickfixDict) -> list[object]:
        return [name, len(entry.fields), sum(len(entry.values) for entry in entry.fields.values())]

    rows = [row(version, sources.quickfix[version]) for version in ORDER]
    rows.append(row(SESSION[0], sources.session))
    doc.table(["Version file", "Fields declared", "Enum values listed"], rows)
    latest = set(sources.latest)
    doc.table(
        ["Set", "Tags"],
        [
            ["Union of all QuickFIX/J files (application + session)", len(sources.bundled)],
            ["FIX Latest fields", len(latest)],
            [
                "FIX Latest codes as written (shared sets repeated per field)",
                sum(len(entry.codes) for entry in sources.latest.values()),
            ],
            ["In both", len(sources.bundled & latest)],
            ["Only in QuickFIX/J → group 2", len(sources.only_bundled)],
            ["Only in FIX Latest → group 3", len(sources.only_latest)],
        ],
    )


def write_quickfix_only(sources: Sources, doc: Document) -> None:
    fix50 = sources.quickfix["FIX.5.0"]
    unreferenced = set(fix50.unreferenced())
    own = [
        tag
        for tag in sources.only_bundled
        if sources.present[tag] == ["FIX.5.0"] and tag >= USER_DEFINED_START
    ]
    retired = [tag for tag in sources.only_bundled if tag not in own]

    doc.text("## 2. Tags only in QuickFIX/J (absent from FIX Latest)")
    doc.text("""
        FIX Latest drops nothing on purpose except what was removed from the
        standard, so a QuickFIX/J tag that FIX Latest lacks is either retired or was
        never standard. Two groups, told apart by their history:
    """)
    doc.text(f"### 2a. Retired standard tags ({len(retired)})")
    doc.text("""
        Present in early versions, dropped by a later one, gone from FIX Latest.
        `FIX50.xml` re-declares many of them without using them (see group 6).
    """)
    rows = []
    for tag in retired:
        name = next(sources.quickfix[version].fields[tag].name for version in sources.present[tag])
        if tag not in fix50.fields:
            unused = "—"
        else:
            unused = "yes" if tag in unreferenced else "no"
        rows.append([label(tag, name), short_list(sources.present[tag]), unused])
    doc.table(["Field", "Declared in", "Declared but unused in FIX50.xml"], rows)
    doc.text("`—` in the last column: not declared in `FIX50.xml` at all.")
    doc.text(f"### 2b. QuickFIX/J's own tags ({len(own)})")
    doc.text("""
        Declared only in `FIX50.xml`, referenced by no message there, absent from
        every other version and from FIX Latest, all in the 5000–9999 user-defined
        range. QuickFIX C++ and QuickFIX/n do not have them.
    """)
    rows = [
        [label(tag, fix50.fields[tag].name), fix50.fields[tag].type, len(fix50.fields[tag].values)]
        for tag in own
    ]
    doc.table(["Field", "Type", "Values listed"], rows)


def write_latest_only(sources: Sources, doc: Document) -> None:
    latest = sources.latest
    only = sources.only_latest
    doc.text("## 3. Tags only in FIX Latest (in no QuickFIX/J file)")
    doc.text(f"""
        {len(only)} tags. Every one is an extension-pack addition: the
        QuickFIX/J files stop at FIX 5.0 SP2 as released, and the standard kept
        growing through EPs applied on top of it.
    """)
    by_dating = Counter((latest[tag].added, latest[tag].added_ep > 0) for tag in only)
    rows = [
        [added, "yes" if has_pack else "no", count]
        for (added, has_pack), count in sorted(by_dating.items())
    ]
    doc.table(["`added`", "Has `addedEP`", "Tags"], rows)
    packs = Counter(latest[tag].added_ep for tag in only)
    doc.text(f"Spread over {len(packs)} extension packs, EP{min(packs)}–EP{max(packs)}. Ranges:")
    buckets = Counter(latest[tag].added_ep // EP_BUCKET * EP_BUCKET for tag in only)
    rows = [
        [f"EP{start}–EP{start + EP_BUCKET - 1}", count] for start, count in sorted(buckets.items())
    ]
    doc.table(["EP range", "Tags"], rows)
    doc.text("""
        By tag range (5000–9999 was the registered user-defined range; 20000–39999
        is reserved for bilateral use; FIX Latest uses neither for standard tags):
    """)
    by_range = Counter(latest_only_range(tag) for tag in only)
    rows = [[name, by_range[name]] for name in LATEST_ONLY_RANGES if by_range[name]]
    doc.table(["Range", "Tags"], rows)
    doc.text("The complete list, by extension pack, is in `tags-inventory.md` §B.")


def latest_only_range(tag: int) -> str:
    """As `tag_range`, with 40000 and up in one row."""
    return "≥ 40000" if tag >= STANDARD_AGAIN else tag_range(tag)


LATEST_ONLY_RANGES = [name for start, name in TAG_RANGES if start < STANDARD_AGAIN] + ["≥ 40000"]


def write_dating(sources: Sources, doc: Document) -> None:
    latest, present = sources.latest, sources.present
    both = sources.application_in_latest
    later, earlier, agree, undated = [], [], [], []
    for tag in both:
        added, first = latest[tag].added, present[tag][0]
        if added not in RANK:
            undated.append(tag)
        elif max(RANK[added], 0) < RANK[first]:
            later.append(tag)
        elif RANK[added] > RANK[first]:
            earlier.append(tag)
        else:
            agree.append(tag)
    session_only = len(sources.bundled & set(latest)) - len(both)

    def dated_rows(tags: list[int]) -> list[list[object]]:
        return [
            [label(tag, latest[tag].name), dating(latest[tag]), present[tag][0]] for tag in tags
        ]

    doc.text("## 4. Dating disagreements: FIX Latest `added` vs. first QuickFIX/J file")
    doc.text(f"""
        Of the {len(both)} tags in both families (application files; the other
        {session_only} are session-only, see group 8), the QuickFIX/J file of the
        `added` version contains the tag for {len(agree)} — counting `FIX.2.7`/`FIX.3.0`
        as `FIX.4.0`, the oldest file; {len(later)} first appear in a *later* file;
        {len(earlier)} in an *earlier* one.
    """)
    doc.text("### 4a. First bundled later than `added`, with an extension pack")
    with_pack = [tag for tag in later if latest[tag].added_ep > 0]
    doc.text(f"""
        {len(with_pack)} tags. `added=X addedEP=n` means EP n was applied on top of X
        and shipped with the next release, so the QuickFIX/J file of X predates it.
        Grouped by the version the pack was applied to and where the tag first ships:
    """)
    packs = defaultdict(list)
    for tag in with_pack:
        packs[(latest[tag].added, present[tag][0])].append(latest[tag].added_ep)
    in_order = sorted(packs.items(), key=lambda item: (RANK[item[0][0]], RANK[item[0][1]]))
    rows = [
        [added, first, len(eps), f"EP{min(eps)}–EP{max(eps)}"]
        for (added, first), eps in in_order
    ]
    doc.table(["`added`", "First QuickFIX/J file", "Tags", "EPs"], rows)
    skipped = [tag for tag in with_pack if RANK[present[tag][0]] - RANK[latest[tag].added] > 1]
    if skipped:
        doc.text("Not in the very next release either:")
        doc.table(["Field", "`added`", "First file"], dated_rows(skipped))
    doc.text("### 4b. First bundled later than `added`, no extension pack")
    no_pack = [tag for tag in later if latest[tag].added_ep == 0]
    doc.text(f"{len(no_pack)} tags where the two sources simply disagree on the version.")
    rows = [
        [
            label(tag, latest[tag].name),
            latest[tag].added,
            present[tag][0],
            "own description says 4.3" if tag == BENCHMARK_CURVE_CURRENCY else "",
        ]
        for tag in no_pack
    ]
    doc.table(["Field", "`added`", "First file", "Note"], rows)
    doc.text("### 4c. First bundled earlier than `added`")
    doc.table(["Field", "`added`", "First file"], dated_rows(earlier))
    if undated:
        doc.text("### 4d. `added` names no bundled version")
        doc.table(["Field", "`added`", "First file"], dated_rows(undated))


def write_presence(sources: Sources, doc: Document) -> None:
    latest, present = sources.latest, sources.present
    application = sorted(sources.application)
    removed = [tag for tag in application if present[tag][-1] != LAST]
    gaps = [tag for tag in application if has_gap([RANK[version] for version in present[tag]])]

    doc.text("## 5. Presence across versions: removals and gaps")
    doc.text(f"### 5a. Tags a later version drops for good ({len(removed)})")
    doc.text("""
        Present in some version, absent from the last bundled one (FIX.5.0SP2).
        FIX Latest records deprecation but never removal, so this list exists only
        because the per-version files do.
    """)
    rows = []
    for tag in removed:
        known = latest.get(tag)
        rows.append([
            label(tag, sources.name(tag)),
            short_list(present[tag]),
            dating(known) if known else "absent",
            known.deprecated if known else "",
        ])
    doc.table(["Field", "Declared in", "FIX Latest", "`deprecated`"], rows)
    doc.text(f"### 5b. Tags missing from a middle version ({len(gaps)})")
    doc.text("Present before and after, absent in between.")
    rows = [[label(tag, sources.name(tag)), short_list(present[tag])] for tag in gaps]
    doc.table(["Field", "Declared in"], rows)


def write_unreferenced(sources: Sources, doc: Document) -> None:
    doc.text("## 6. Declared but unreferenced fields")
    doc.text("""
        A `<field>` in `<fields>` that no header, trailer, message or component of
        the same file uses. Harmless for lookups, but it means the file's `<fields>`
        list is not the set of tags that version's messages can carry.
    """)
    doc.text("""
        The 5.x application files have empty `<header>`/`<trailer>` blocks (the
        session layer lives in `FIXT11.xml`), so their session fields count as
        unreferenced by construction; they are split out below.
    """)
    session = sources.session.fields
    rows = []
    application: dict[str, list[int]] = {}
    for version in ORDER:
        unreferenced = sources.quickfix[version].unreferenced()
        in_session = [tag for tag in unreferenced if tag in session]
        rest = [tag for tag in unreferenced if tag not in session]
        application[version] = rest
        absent = [tag for tag in rest if tag not in sources.latest]
        rows.append([version, len(unreferenced), len(in_session), len(rest), len(absent)])
    unreferenced = sources.session.unreferenced()
    application[SESSION[0]] = unreferenced
    absent = [tag for tag in unreferenced if tag not in sources.latest]
    rows.append([SESSION[0], len(unreferenced), "—", len(unreferenced), len(absent)])
    doc.table(
        [
            "Version file",
            "Unreferenced",
            "…session-layer (in FIXT11)",
            "…application",
            "…of those, absent from FIX Latest",
        ],
        rows,
    )
    doc.text("Unreferenced application fields per file:")
    for version in [*ORDER, SESSION[0]]:
        tags = application[version]
        if tags:
            fields = sources.quickfix.get(version, sources.session).fields
            names = ", ".join(label(tag, fields[tag].name) for tag in tags)
            doc.text(f"**{version}** ({len(tags)}): {names}")


def write_names_and_types(sources: Sources, doc: Document) -> None:
    latest, present, quickfix = sources.latest, sources.present, sources.quickfix
    application = sorted(sources.application)

    doc.text("## 7. Names and types that differ between sources")
    rows = []
    for tag in application:
        names = [(version, quickfix[version].fields[tag].name) for version in present[tag]]
        if len({name for _, name in names}) > 1:
            in_latest = label(tag, latest[tag].name) if tag in latest else "absent"
            rows.append([label(tag, names[-1][1]), runs(names), in_latest])
    doc.text(f"### 7a. Name differs between QuickFIX/J versions ({len(rows)})")
    doc.table(["Field (newest file)", "Names by version", "FIX Latest"], rows)

    rows = []
    for tag in sources.application_in_latest:
        newest = present[tag][-1]
        name = quickfix[newest].fields[tag].name
        if name != latest[tag].name:
            rows.append([label(tag, name), SHORT[newest], label(tag, latest[tag].name)])
    doc.text(f"### 7b. Latest QuickFIX/J name differs from FIX Latest ({len(rows)})")
    doc.text("Compared against the newest QuickFIX/J file that has the tag.")
    doc.table(["Field (QuickFIX/J)", "in", "FIX Latest name"], rows)

    rows = []
    for tag in application:
        types = [(version, quickfix[version].fields[tag].type) for version in present[tag]]
        if len({kind.upper() for _, kind in types}) > 1:
            in_latest = latest[tag].type if tag in latest else "absent"
            rows.append([label(tag, sources.name(tag)), runs(types), in_latest])
    doc.text(f"### 7c. Type differs between QuickFIX/J versions ({len(rows)})")
    doc.table(["Field", "Types by version", "FIX Latest type"], rows)


def write_session_layer(sources: Sources, doc: Document) -> None:
    latest, quickfix, session = sources.latest, sources.quickfix, sources.session
    session_tags = sorted(session.fields)

    doc.text("## 8. Session layer: `FIXT11.xml` against the 5.x application files")
    doc.text("""
        FIXT 1.1 carries the session layer for FIX 5.0 and later. Its fields are
        declared in `FIXT11.xml`; the application files may or may not repeat them.
    """)
    rows = []
    for version in APPLICATION_5X:
        missing = [tag for tag in session_tags if tag not in quickfix[version].fields]
        rows.append([version, len(session_tags) - len(missing), len(missing)])
    doc.table(["Application file", "FIXT11 fields it also declares", "…it lacks"], rows)

    rows = []
    for tag in session_tags:
        declared = [version for version in APPLICATION_5X if tag in quickfix[version].fields]
        if len(declared) < len(APPLICATION_5X):
            rows.append([
                label(tag, session.fields[tag].name),
                short_list(declared) or "none",
                dating(latest[tag]) if tag in latest else "absent",
            ])
    doc.text(f"FIXT11 fields absent from at least one 5.x application file ({len(rows)}):")
    doc.table(["Field", "Declared in", "FIX Latest"], rows)

    only = sorted(tag for tag in session.fields if tag not in sources.application)
    names = ", ".join(label(tag, session.fields[tag].name) for tag in only)
    doc.text(f"FIXT11 fields in no application file at all ({len(only)}): {names}")

    packed = [tag for tag in session_tags if tag in latest and latest[tag].added_ep > 0]
    doc.text(f"FIXT11 fields that FIX Latest dates with an extension pack ({len(packed)}):")
    rows = [[label(tag, session.fields[tag].name), dating(latest[tag])] for tag in packed]
    doc.table(["Field", "FIX Latest"], rows)

    conflicts = []
    for tag in session_tags:
        ours = session.fields[tag]
        for version in APPLICATION_5X:
            theirs = quickfix[version].fields.get(tag)
            if theirs and (ours.name != theirs.name or ours.type.upper() != theirs.type.upper()):
                conflicts.append([
                    label(tag, ours.name),
                    version,
                    f"{ours.name} {ours.type}",
                    f"{theirs.name} {theirs.type}",
                ])
    table = (
        markdown_table(["Field", "File", "FIXT11", "Application"], conflicts)
        if conflicts
        else "none."
    )
    doc.text(f"Name or type conflicts between FIXT11 and an application file: {table}")


def write_enum_values(sources: Sources, doc: Document) -> None:
    latest = sources.latest
    doc.text("## 9. Enum values")
    doc.text("""
        QuickFIX/J lists a field's values per version; FIX Latest lists each
        field's current code set with pedigree. `ownCodeSet` marks the field a set is
        named after; other fields *share* it (FIX Latest repeats the codes under each).
    """)
    sets = sources.code_sets
    doc.table(
        ["FIX Latest fields", "Count"],
        [
            ["with their own code set", sum(entry.own_code_set for entry in latest.values())],
            [
                "sharing another field's set",
                sum(bool(entry.codes) and not entry.own_code_set for entry in latest.values()),
            ],
            ["with no code set", sum(not entry.codes for entry in latest.values())],
            ["distinct code sets by content", len(sets)],
            ["code rows if each set is stored once", sum(len(key) for key in sets)],
        ],
    )
    write_values_only_in_quickfix(sources, doc)
    write_open_ended_samples(sources, doc)
    write_boolean_listings(sources, doc)
    write_shared_sets(sources, doc)
    write_own_set_gaps(sources, doc)
    write_msg_type(sources, doc)
    write_undated_codes(sources, doc)


def write_values_only_in_quickfix(sources: Sources, doc: Document) -> None:
    doc.text("### 9a. Values QuickFIX/J lists that FIX Latest does not")
    found = values_absent_from_latest(sources)
    only_fix50, elsewhere = [], []
    for tag, value, by_description in found:
        versions = {version for listed in by_description.values() for version in listed}
        descriptions = "; ".join(
            f"{description} ({short_list(listed)})"
            for description, listed in by_description.items()
        )
        row = [label(tag, sources.latest[tag].name), value, descriptions]
        (only_fix50 if versions == {"FIX.5.0"} else elsewhere).append(row)
    header = ["Field", "Value", "QuickFIX/J descriptions by file"]
    doc.text(f"{len(found)} (tag, value) pairs on fields that FIX Latest does enumerate.")
    doc.text(f"Listed only in `FIX50.xml` ({len(only_fix50)}):")
    doc.table(header, only_fix50)
    doc.text(f"Elsewhere ({len(elsewhere)}) — retired values, or values FIX Latest never had:")
    doc.table(header, elsewhere)


def write_open_ended_samples(sources: Sources, doc: Document) -> None:
    rows = []
    for tag in sources.application_in_latest:
        if sources.latest[tag].codes:
            continue
        listed_in: dict[str, list[str]] = {}
        for version in sources.present[tag]:
            for value, _ in sources.quickfix[version].fields[tag].values:
                listed_in.setdefault(value, []).append(version)
        if listed_in:
            samples = ", ".join(
                f"`{value}` ({short_list(versions)})" for value, versions in listed_in.items()
            )
            rows.append([label(tag, sources.latest[tag].name), sources.latest[tag].type, samples])
    doc.text(
        f"### 9b. QuickFIX/J lists values for fields FIX Latest does not enumerate ({len(rows)})"
    )
    doc.text("Open-ended fields where a QuickFIX/J file gives sample values. Not a closed set.")
    doc.table(["Field", "FIX Latest type", "Values listed"], rows)


def write_boolean_listings(sources: Sources, doc: Document) -> None:
    latest = sources.latest
    doc.text("### 9c. Boolean fields: Y/N listed inconsistently")
    booleans = [tag for tag in sources.application_in_latest if latest[tag].type == "Boolean"]
    rows = []
    incomplete = 0
    for tag in booleans:
        states = []
        for version in sources.present[tag]:
            listed = sources.quickfix[version].listed(tag)
            if listed >= {"Y", "N"}:
                states.append("YN")
            else:
                states.append("partial" if listed else "—")
                incomplete += 1
        if len(set(states)) > 1 or "—" in states:
            listing = zip(sources.present[tag], states, strict=True)
            per_version = ", ".join(f"{SHORT[version]}:{state}" for version, state in listing)
            codes = "; ".join(f"{code.value} {dating(code)}" for code in latest[tag].codes)
            rows.append([label(tag, latest[tag].name), per_version, codes])
    doc.text(f"""
        {len(booleans)} Boolean fields; {incomplete} (field, version) pairs list no or partial Y/N.
        Fields where the listing varies by version:
    """)
    doc.table(["Field", "Y/N listed per version", "FIX Latest codes"], rows)


def write_shared_sets(sources: Sources, doc: Document) -> None:
    latest = sources.latest
    doc.text("### 9d. Fields sharing a code set: what QuickFIX/J lists")
    doc.text("""
        A shared set's codes belong to the field the set is named after. QuickFIX/J
        files list values for the sharing field inconsistently — often nothing.
    """)
    rows = []
    for tag in sources.application_in_latest:
        entry = latest[tag]
        if not entry.codes or entry.own_code_set:
            continue
        owner = next(
            (
                other
                for users in sources.code_sets.values()
                if tag in users
                for other in users
                if latest[other].own_code_set
            ),
            None,
        )
        owner_name = label(owner, latest[owner].name) if owner is not None else "?"
        per_file = ", ".join(
            f"{SHORT[version]}:{len(sources.quickfix[version].fields[tag].values)}"
            for version in sources.present[tag]
        )
        rows.append([label(tag, entry.name), owner_name, len(entry.codes), per_file])
    doc.table(
        ["Field", "Set owner", "Codes in FIX Latest", "Values listed per QuickFIX/J file"], rows
    )


def write_own_set_gaps(sources: Sources, doc: Document) -> None:
    doc.text("### 9e. Own code sets: values dated at or before a version but not listed there")
    missing_rows, gap_rows = [], []
    for tag in sources.application_in_latest:
        entry = sources.latest[tag]
        if not entry.own_code_set or entry.type == "Boolean" or tag == MSG_TYPE:
            continue
        present = sources.present[tag]
        for code in entry.codes:
            if code.added not in RANK or code.added_ep > 0:
                continue
            listed = [
                version
                for version in present
                if code.value in sources.quickfix[version].listed(tag)
            ]
            due = [version for version in present if RANK[version] >= RANK[code.added]]
            missing = [version for version in due if version not in listed]
            if missing:
                missing_rows.append(
                    [label(tag, entry.name), code.value, dating(code), short_list(missing)]
                )
            if has_gap([RANK[version] for version in listed]):
                gap_rows.append([label(tag, entry.name), code.value, short_list(listed)])
    doc.text(f"""
        {len(missing_rows)} (field, value) pairs with a non-EP `added` ≤ some version that
        lists the field but not the value. These are the cases the bundled tables
        restore from FIX Latest at build time (`src/dictionary/bundle.rs`).
    """)
    doc.table(
        ["Field", "Value", "`added`", "Missing from"], missing_rows, limit=MISSING_VALUES_SHOWN
    )
    doc.text(
        f"Values listed in a version, missing from the next, listed again later ({len(gap_rows)}):"
    )
    doc.table(["Field", "Value", "Listed in"], gap_rows)


def write_msg_type(sources: Sources, doc: Document) -> None:
    codes = sources.latest[MSG_TYPE].codes
    oldest = [code for code in codes if code.added == "FIX.2.7" and code.added_ep == 0]
    packed = [code for code in codes if code.added_ep > 0]
    undated = [code for code in codes if not code.added]
    doc.text("### 9f. MsgType(35)")
    counts = f"{len(packed)} with an EP; {len(undated)} without `added`."
    doc.text(f"""
        FIX Latest lists {len(codes)} message types. {len(oldest)} are dated `FIX.2.7`
        with no EP regardless of their real age; {counts}
        QuickFIX/J files list the message types of that version, so the per-version
        picture comes from them, not from FIX Latest's dating.
    """)
    rows = []
    for code in oldest:
        listed = [
            version for version in ORDER if code.value in sources.quickfix[version].listed(MSG_TYPE)
        ]
        if listed and listed[0] != ORDER[0]:
            rows.append([code.value, code.name, listed[0]])
    doc.text(
        f"Message types dated `FIX.2.7` whose first QuickFIX/J listing is later ({len(rows)}):"
    )
    doc.table(["MsgType", "Name", "First file"], rows)
    if undated:
        names = ", ".join(f"{code.value} {code.name}" for code in undated)
        doc.text(f"Codes with no `added`: {names}")


def write_undated_codes(sources: Sources, doc: Document) -> None:
    rows = [
        [label(tag, entry.name), code.value, code.name]
        for tag, entry in sources.latest.items()
        for code in entry.codes
        if not code.added
    ]
    doc.text(f"### 9g. FIX Latest codes with no `added` ({len(rows)})")
    doc.table(["Field", "Value", "Name"], rows)


def write_tag_ranges(sources: Sources, doc: Document) -> None:
    latest = sources.latest
    doc.text("## 10. User-defined tag ranges")
    doc.text("""
        The [FIX specification](https://www.fixtrading.org/online-specification/introduction/)
        assigns the non-standard and reserved ranges as follows:
    """)
    doc.text("""
        - 5000–9999: registered inter-firm user-defined fields; now exhausted.
        - 10000–19999: internal use within one firm; no registration required.
        - 20000–39999: bilateral user-defined fields; no registration required.
        - 40000–49999: reserved by the FIX Global Technical Committee and used for
          standard fields.
        - 50000 and above: reserved by the FIX Global Technical Committee; private
          implementations should not use them.
    """)
    names = [name for _, name in TAG_RANGES]
    rows = []
    for source, tags in (("QuickFIX/J union", sources.bundled), ("FIX Latest", set(latest))):
        counts = Counter(tag_range(tag) for tag in tags)
        rows.append([source, *(counts.get(name, 0) for name in names)])
    doc.table(["Source", *names], rows)
    high = sorted(tag for tag in sources.bundled if tag >= USER_DEFINED_START)
    doc.text("QuickFIX/J tags ≥ 5000: " + ", ".join(label(tag, sources.name(tag)) for tag in high))
    standard_high = sorted(tag for tag in latest if tag >= STANDARD_AGAIN)
    examples = ", ".join(
        f"{label(tag, latest[tag].name)} {dating(latest[tag])}" for tag in standard_high[:6]
    )
    low, top = min(standard_high), max(standard_high)
    doc.text(f"""
        FIX Latest tags ≥ 40000 ({len(standard_high)}, {low}–{top}, all extension-pack additions),
        e.g. {examples} …
    """)


def write_pedigree_oddities(sources: Sources, doc: Document) -> None:
    latest, present = sources.latest, sources.present
    deprecated = [tag for tag, entry in latest.items() if entry.deprecated]
    added_values = {entry.added for entry in latest.values()}
    doc.text("## 11. FIX Latest pedigree oddities")
    doc.table(
        ["Fact", "Count"],
        [
            ["fields without `added`", sum(not entry.added for entry in latest.values())],
            ["fields with `deprecated`", len(deprecated)],
            ["distinct `added` values", len(added_values)],
            ["largest `addedEP`", max(entry.added_ep for entry in latest.values())],
        ],
    )
    in_use = sorted(added_values - {""}, key=lambda added: RANK.get(added, 99))
    doc.text("`added` values in use: " + ", ".join(in_use))
    still = [tag for tag in deprecated if tag in sources.application and LAST in present[tag]]
    doc.text(f"Deprecated fields still declared in the last QuickFIX/J file ({len(still)}):")
    rows = [
        [label(tag, latest[tag].name), latest[tag].deprecated, short_list(present[tag])]
        for tag in still
    ]
    doc.table(["Field", "`deprecated`", "Declared in"], rows, limit=DEPRECATED_SHOWN)


# --- docs/tags-inventory.md -------------------------------------------------


def write_inventory(sources: Sources, doc: Document) -> None:
    latest = sources.latest
    doc.text("# FIX tag inventory")
    doc.text("""
        Generated by `tools/tag_inventory.py`; do not edit by hand. Companion to
        [`tags.md`](tags.md), which explains the groups.
    """)
    doc.text("## A. Every tag in a QuickFIX/J file")
    doc.text("""
        Columns: the QuickFIX/J files that declare the tag (application files by
        version, `T1.1` for `FIXT11.xml`), the name in the newest file, and FIX
        Latest's name and pedigree. `—` in the FIX Latest column: absent (group 2).
    """)
    rows = []
    for tag in sorted(sources.bundled):
        present = sources.present[tag]
        in_session = tag in sources.session.fields
        files = [SHORT[version] for version in present]
        if in_session:
            files.append(SHORT[SESSION[0]])
        newest = (sources.quickfix[present[-1]] if present else sources.session).fields[tag]
        known = latest.get(tag)
        in_latest = f"{label(tag, known.name)} · {dating(known)}" if known else "—"
        difference = "name differs" if known and known.name != newest.name else ""
        rows.append([label(tag, newest.name), newest.type, ", ".join(files), in_latest, difference])
    doc.table(["Field (newest file)", "Type", "Declared in", "FIX Latest", "Difference"], rows)
    doc.text("## B. Every tag only in FIX Latest, by extension pack")
    by_pack = defaultdict(list)
    for tag in sources.only_latest:
        by_pack[(latest[tag].added, latest[tag].added_ep)].append(tag)
    for (added, pack), tags in sorted(by_pack.items(), key=lambda kv: (kv[0][1], kv[0][0])):
        names = ", ".join(label(tag, latest[tag].name) for tag in tags)
        doc.text(f"**{added} EP{pack}** ({len(tags)}): {names}")


SECTIONS = [
    write_sources,
    write_counts,
    write_quickfix_only,
    write_latest_only,
    write_dating,
    write_presence,
    write_unreferenced,
    write_names_and_types,
    write_session_layer,
    write_enum_values,
    write_tag_ranges,
    write_pedigree_oddities,
]


def main() -> None:
    sources = Sources.read()
    tags, inventory = Document(), Document()
    for section in SECTIONS:
        section(sources, tags)
    write_inventory(sources, inventory)

    DOCS.mkdir(exist_ok=True)
    for name, doc in (("tags.md", tags), ("tags-inventory.md", inventory)):
        (DOCS / name).write_text(doc.render())
        print(f"wrote {DOCS / name} ({len(doc.lines)} lines)")


if __name__ == "__main__":
    main()
