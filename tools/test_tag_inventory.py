"""Verify the inventory against independently read, public XML sources."""

from pathlib import Path
import subprocess
import sys
import tempfile
import unittest
import xml.etree.ElementTree as ET

import tag_inventory as inventory


class InventoryTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.directory = inventory.ROOT / "xml/standard"
        cls.applications, cls.transport = inventory.load_sources(cls.directory)

    def test_transport_is_independent_of_application_chronology(self):
        self.assertEqual(len(inventory.APPLICATIONS), 8)
        self.assertNotIn(inventory.TRANSPORT, inventory.APPLICATIONS)
        self.assertEqual(self.transport.version, "FIXT.1.1")
        self.assertEqual(self.transport.messages["A"], "Logon")
        self.assertNotIn("D", self.transport.messages)
        self.assertIn("D", self.transport.fields[35].values)

    def test_sources_preserve_raw_field_and_message_membership(self):
        for source in [*self.applications, self.transport]:
            with self.subTest(source=source.version):
                root = ET.parse(self.directory / source.filename).getroot()
                original = {int(field.attrib["number"])
                            for field in root.find("fields")}
                self.assertEqual(set(source.fields), original)
                self.assertEqual(set(source.messages), {
                    message.attrib["msgtype"] for message in root.find("messages")})

    def test_sp2_does_not_gain_transport_only_fields(self):
        sp2 = self.applications[-1]
        self.assertNotIn(1137, sp2.fields)
        self.assertNotIn(1407, sp2.fields)
        self.assertNotIn("A", sp2.messages)
        self.assertIn(1137, self.transport.fields)
        self.assertIn(1407, self.transport.fields)
        self.assertIn("A", self.transport.messages)
        application_tags = set().union(*(source.fields for source in self.applications))
        self.assertEqual(set(self.transport.fields) - application_tags,
                         {1407, 1408, 1409, 1410})

    def test_enum_identity_ignores_changing_descriptions(self):
        sources = [
            inventory.Source("FIX.4.3", "first.xml",
                             {327: inventory.Field("HaltReason", {"P": "Earlier"})}, {}),
            inventory.Source("FIX.4.4", "second.xml",
                             {327: inventory.Field("HaltReason", {"P": "Later"})}, {}),
        ]
        metadata = ET.fromstring('<metadata><fields><field id="327"/></fields></metadata>')
        absent = inventory.values_absent_from_latest(sources, metadata)
        self.assertEqual(absent, [(327, "P", {
            "Earlier": ["FIX.4.3"], "Later": ["FIX.4.4"]})])

    def test_metadata_values_are_not_reported_as_absent(self):
        source = inventory.Source("FIX.4.4", "synthetic.xml",
                                  {54: inventory.Field("Side", {"1": "Buy", "2": "Sell"})}, {})
        metadata = ET.fromstring('<metadata><fields><field id="54">'
                                 '<code value="1"/></field></fields></metadata>')
        self.assertEqual(inventory.values_absent_from_latest([source], metadata),
                         [(54, "2", {"Sell": ["FIX.4.4"]})])

    def test_cli_generates_and_checks_a_temporary_document(self):
        with tempfile.TemporaryDirectory() as directory:
            output = Path(directory) / "tags.md"
            command = [sys.executable, str(Path(inventory.__file__)),
                       "--xml-dir", str(self.directory), "--output", str(output)]
            missing = subprocess.run([*command, "--check"], capture_output=True)
            self.assertEqual(missing.returncode, 1)
            subprocess.run(command, check=True, capture_output=True)
            document = output.read_text(encoding="utf-8")
            self.assertIn("| FIXT.1.1 | `FIXT11.xml` |", document)
            self.assertIn("| DefaultApplExtID(1407) |", document)
            subprocess.run([*command, "--check"], check=True, capture_output=True)
            output.write_text(document + "Changed\n", encoding="utf-8")
            changed = subprocess.run([*command, "--check"], capture_output=True)
            self.assertEqual(changed.returncode, 1)
            self.assertIn(b"differs; regenerate", changed.stderr)

    def test_transport_only_tag_has_no_application_presence(self):
        document = inventory.render(self.directory)
        row = next(line for line in document.splitlines()
                   if line.startswith("| DefaultApplExtID(1407) |"))
        columns = [column.strip() for column in row.split("|")]
        self.assertEqual(columns[2:10], [""] * 8)
        self.assertEqual(columns[10:12], ["✓", "✓"])

    def test_markdown_cells_escape_xml_descriptions(self):
        self.assertEqual(inventory.markdown("one|two\nthree"), "one\\|two three")


if __name__ == "__main__":
    unittest.main()
