#!/usr/bin/env python3
"""Project OrchestraFIXLatest.xml onto the metadata the bundled dictionaries use.

Usage: tools/orchestra_projection.py OrchestraFIXLatest.xml xml/standard/FIXLatest-metadata.xml

Keeps only: field id, name, base type, added, addedEP, deprecated, ownCodeSet,
first SYNOPSIS; per field, its codes' value, name, added, addedEP, deprecated,
first SYNOPSIS; message type, name, category, first SYNOPSIS. The source hash is
checked so the output is reproducible from the pinned upstream file alone.

`ownCodeSet` is a source-specific convention, not an Orchestra one: the schema
links a field to its code set by type name, but a set that belongs to one field
is given that field's id. `validate` holds the convention to its meaning.
"""

import hashlib
import sys
import xml.etree.ElementTree as ET
from dataclasses import dataclass
from dataclasses import field as dataclass_field
from xml.sax.saxutils import escape

SOURCE_SHA256 = "1bf82a22423d6504fbb77c4c8c522a4efefb32aec19b630c138860ccddf8228d"
SOURCE_URL = (
    "https://github.com/FIXTradingCommunity/orchestrations/blob/"
    "cd24169a2abd8daba7c360987c7a46ca11873a12/FIX%20Standard/OrchestraFIXLatest.xml"
)
NS = "{http://fixprotocol.io/2020/orchestra/repository}"
U16_MAX = 65535


class Invalid(Exception):
    """The source does not meet a contract the projection depends on."""


@dataclass
class Code:
    value: str
    name: str
    added: str
    added_ep: str
    deprecated: str
    synopsis: str


@dataclass
class CodeSet:
    id: str
    name: str
    type: str
    codes: list[Code] = dataclass_field(default_factory=list)


@dataclass
class Field:
    id: str
    name: str
    type: str
    added: str
    added_ep: str
    deprecated: str
    synopsis: str


@dataclass
class Message:
    msg_type: str
    name: str
    category: str
    synopsis: str


@dataclass
class Repository:
    source: str
    code_sets: dict[str, CodeSet]
    fields: list[Field]
    messages: list[Message]

    def code_set_of(self, field: Field) -> CodeSet | None:
        return self.code_sets.get(field.type)

    def owns(self, field: Field) -> bool:
        """The field the set is named after, by the id convention above."""
        code_set = self.code_set_of(field)
        return code_set is not None and code_set.id == field.id


def extension_pack(raw: str) -> int:
    """`addedEP` as a number: 0 when no positive extension pack is recorded.

    Absent and `-1` are both 0 — nothing is claimed about why the source
    records `-1`, only that it names no pack. Every other shape is a source
    the projection refuses to guess about.
    """
    if raw == "":
        return 0
    try:
        number = int(raw)
    except ValueError:
        raise Invalid(f'addedEP="{raw}" is not a number') from None
    if number == -1:
        return 0
    if number <= 0:
        raise Invalid(f'addedEP="{raw}" is neither positive nor -1')
    if number > U16_MAX:
        raise Invalid(f'addedEP="{raw}" does not fit in u16')
    return number


def section(root: ET.Element, name: str) -> ET.Element:
    """A required top-level section; absent means the source is not Orchestra."""
    found = root.find(NS + name)
    if found is None:
        raise Invalid(f"missing <{name}> section")
    return found


def attr(element: ET.Element, name: str) -> str:
    """An attribute as text; absent and empty are the same to every caller."""
    return element.get(name) or ""


def synopsis(element: ET.Element) -> str:
    # Direct annotation only: a message's structure annotates its members too.
    annotation = element.find(NS + "annotation")
    if annotation is None:
        return ""
    for doc in annotation.findall(NS + "documentation"):
        if doc.get("purpose") != "SYNOPSIS":
            continue
        # Upstream indents the text with the XML; the words are the content.
        text = " ".join("".join(doc.itertext()).split())
        if text:
            return text
    return ""


def parse(data: bytes) -> Repository:
    root = ET.fromstring(data)
    code_sets = {}
    for element in section(root, "codeSets"):
        code_set = CodeSet(
            id=attr(element, "id"),
            name=attr(element, "name"),
            type=attr(element, "type"),
        )
        for code in element.iter(NS + "code"):
            code_set.codes.append(
                Code(
                    value=attr(code, "value"),
                    name=attr(code, "name"),
                    added=attr(code, "added"),
                    added_ep=attr(code, "addedEP"),
                    deprecated=attr(code, "deprecated"),
                    synopsis=synopsis(code),
                )
            )
        code_sets[code_set.name] = code_set
    fields = [
        Field(
            id=attr(element, "id"),
            name=attr(element, "name"),
            type=attr(element, "type"),
            added=attr(element, "added"),
            added_ep=attr(element, "addedEP"),
            deprecated=attr(element, "deprecated"),
            synopsis=synopsis(element),
        )
        for element in section(root, "fields")
    ]
    messages = [
        Message(
            msg_type=attr(element, "msgType"),
            name=attr(element, "name"),
            category=attr(element, "category"),
            synopsis=synopsis(element),
        )
        for element in section(root, "messages")
    ]
    fields.sort(key=lambda f: int(f.id))
    messages.sort(key=lambda m: m.msg_type)
    return Repository(attr(root, "version"), code_sets, fields, messages)


def validate(repo: Repository) -> None:
    for field in repo.fields:
        try:
            extension_pack(field.added_ep)
        except Invalid as bad:
            raise Invalid(f"field {field.id}: {bad}") from None
    for code_set in repo.code_sets.values():
        for code in code_set.codes:
            try:
                extension_pack(code.added_ep)
            except Invalid as bad:
                raise Invalid(f"{code_set.name} = {code.value}: {bad}") from None

    # A set named after a field must be reachable from it, or `ownCodeSet`
    # would claim ownership nothing in the source supports. Other fields may
    # share the set — 216 of them do — which is why id decides and use does not.
    by_id = {field.id: field for field in repo.fields}
    for code_set in repo.code_sets.values():
        owner = by_id.get(code_set.id)
        if owner is not None and owner.type != code_set.name:
            raise Invalid(
                f"{code_set.name} carries id {code_set.id}, but field "
                f"{owner.id} references {owner.type or 'no code set'}"
            )


def attrs(pairs: list[tuple[str, str]]) -> str:
    return "".join(
        f' {key}="{escape(value, {chr(34): "&quot;"})}"'
        for key, value in pairs
        if value
    )


def ep_text(raw: str) -> str:
    """Emitted only when a pack is named; absent and `-1` say the same nothing."""
    number = extension_pack(raw)
    return str(number) if number else ""


def emit(repo: Repository) -> str:
    out = [
        '<?xml version="1.0" encoding="UTF-8"?>',
        "<!--",
        "  Generated by tools/orchestra_projection.py; do not edit.",
        f"  Source: {SOURCE_URL}",
        f"  Source SHA-256: {SOURCE_SHA256}",
        "  Copyright (c) FIX Protocol Ltd. Licensed under the Apache License, Version 2.0.",
        "-->",
        f"<metadata{attrs([('source', repo.source)])}>",
        "  <fields>",
    ]
    for field in repo.fields:
        code_set = repo.code_set_of(field)
        base_type = code_set.type if code_set is not None else field.type
        head = attrs(
            [
                ("id", field.id),
                ("name", field.name),
                ("type", base_type),
                ("added", field.added),
                ("addedEP", ep_text(field.added_ep)),
                ("deprecated", field.deprecated),
                ("ownCodeSet", "true" if repo.owns(field) else ""),
                ("synopsis", field.synopsis),
            ]
        )
        codes = code_set.codes if code_set is not None else []
        if not codes:
            out.append(f"    <field{head}/>")
            continue
        out.append(f"    <field{head}>")
        for code in codes:
            body = attrs(
                [
                    ("value", code.value),
                    ("name", code.name),
                    ("added", code.added),
                    ("addedEP", ep_text(code.added_ep)),
                    ("deprecated", code.deprecated),
                    ("synopsis", code.synopsis),
                ]
            )
            out.append(f"      <code{body}/>")
        out.append("    </field>")
    out += ["  </fields>", "  <messages>"]
    for message in repo.messages:
        body = attrs(
            [
                ("msgType", message.msg_type),
                ("name", message.name),
                ("category", message.category),
                ("synopsis", message.synopsis),
            ]
        )
        out.append(f"    <message{body}/>")
    out += ["  </messages>", "</metadata>", ""]
    return "\n".join(out)


def main(source_path: str, output_path: str) -> None:
    with open(source_path, "rb") as source:
        data = source.read()
    digest = hashlib.sha256(data).hexdigest()
    if digest != SOURCE_SHA256:
        sys.exit(f"{source_path}: sha256 {digest}, expected {SOURCE_SHA256}")
    try:
        repo = parse(data)
        validate(repo)
        text = emit(repo)
    except Invalid as bad:
        sys.exit(f"{source_path}: {bad}")
    with open(output_path, "w", encoding="utf-8", newline="\n") as output:
        output.write(text)


if __name__ == "__main__":
    if len(sys.argv) != 3:
        sys.exit(__doc__)
    main(sys.argv[1], sys.argv[2])
