"""Contract tests for the Orchestra projection.

Run with `python3 -m unittest discover -s tools`. They exercise `parse`,
`validate` and `emit` on synthetic XML, which is why the pinned-source hash
check lives in `main` alone.
"""

import unittest

from orchestra_projection import Invalid, emit, parse, validate

TEMPLATE = """<?xml version="1.0" encoding="UTF-8"?>
<fixr:repository xmlns:fixr="http://fixprotocol.io/2020/orchestra/repository"
                 version="FIX.Latest_EP999">
  <fixr:codeSets>{code_sets}</fixr:codeSets>
  <fixr:fields>{fields}</fixr:fields>
  <fixr:messages>
    <fixr:message msgType="D" name="NewOrderSingle" category="SingleGeneralOrderHandling"/>
  </fixr:messages>
</fixr:repository>
"""

# Side(54) owns its set; PartyRole(452) shares it. Together they are the whole
# ownership question in four lines.
SIDE_SET = """
    <fixr:codeSet id="54" name="SideCodeSet" type="char">
      <fixr:code value="1" name="Buy" added="FIX.4.0"/>
      <fixr:code value="2" name="Sell" added="FIX.4.2" addedEP="26"/>
    </fixr:codeSet>"""
SIDE_FIELD = '\n    <fixr:field id="54" name="Side" type="SideCodeSet" added="FIX.4.0"/>'
SHARING_FIELD = (
    '\n    <fixr:field id="452" name="PartyRole" type="SideCodeSet" added="FIX.4.3"/>'
)


def document(code_sets: str = SIDE_SET, fields: str = SIDE_FIELD + SHARING_FIELD) -> bytes:
    return TEMPLATE.format(code_sets=code_sets, fields=fields).encode()


def checked(source: bytes):
    repo = parse(source)
    validate(repo)
    return repo


class ExtensionPack(unittest.TestCase):
    def test_absent_and_minus_one_are_accepted_and_emitted_as_nothing(self):
        field = '\n    <fixr:field id="54" name="Side" type="SideCodeSet" addedEP="-1"/>'
        text = emit(checked(document(fields=field)))
        self.assertNotIn('addedEP="-1"', text)
        self.assertNotIn('addedEP="0"', text)

    def test_a_positive_pack_survives_to_the_output(self):
        sell = 'value="2" name="Sell" added="FIX.4.2" addedEP="26"'
        self.assertIn(sell, emit(checked(document())))

    def test_rejected_shapes_on_a_field(self):
        for raw in ("0", "-2", "x", "70000"):
            with self.subTest(raw=raw):
                field = f'\n    <fixr:field id="54" name="Side" addedEP="{raw}"/>'
                with self.assertRaises(Invalid) as refusal:
                    checked(document(fields=field))
                self.assertIn("field 54", str(refusal.exception))

    def test_rejected_shapes_on_a_code(self):
        for raw in ("0", "-2", "x", "70000"):
            with self.subTest(raw=raw):
                code_set = f"""
    <fixr:codeSet id="54" name="SideCodeSet" type="char">
      <fixr:code value="1" name="Buy" addedEP="{raw}"/>
    </fixr:codeSet>"""
                with self.assertRaises(Invalid) as refusal:
                    checked(document(code_sets=code_set, fields=SIDE_FIELD))
                self.assertIn("SideCodeSet = 1", str(refusal.exception))


class Ownership(unittest.TestCase):
    def test_the_field_the_set_is_named_after_owns_it_and_the_other_does_not(self):
        text = emit(checked(document()))
        self.assertIn('id="54" name="Side" type="char" added="FIX.4.0" ownCodeSet="true"', text)
        self.assertIn('id="452" name="PartyRole" type="char" added="FIX.4.3"', text)
        self.assertNotIn('name="PartyRole" type="char" added="FIX.4.3" ownCodeSet', text)

    def test_a_shared_set_still_carries_its_codes_under_both_fields(self):
        self.assertEqual(emit(checked(document())).count('value="1" name="Buy"'), 2)

    def test_a_set_named_after_a_field_that_does_not_reference_it_is_refused(self):
        with self.assertRaises(Invalid) as refusal:
            side = '\n    <fixr:field id="54" name="Side" type="QtyCodeSet"/>'
            checked(document(fields=SHARING_FIELD + side))
        self.assertIn("SideCodeSet carries id 54", str(refusal.exception))

    def test_a_field_with_no_code_set_is_neither_owner_nor_sharer(self):
        field = '\n    <fixr:field id="60" name="TransactTime" type="UTCTimestamp"/>'
        text = emit(checked(document(fields=SIDE_FIELD + field)))
        self.assertIn('<field id="60" name="TransactTime" type="UTCTimestamp"/>', text)


class Sections(unittest.TestCase):
    def test_a_source_without_the_sections_is_not_orchestra(self):
        with self.assertRaises(Invalid):
            parse(b'<fixr:repository xmlns:fixr="http://fixprotocol.io/2020/orchestra/repository"/>')


if __name__ == "__main__":
    unittest.main()
