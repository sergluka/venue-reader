import unittest

from tag_inventory import (
    LatestCode,
    LatestField,
    QuickfixDict,
    QuickfixField,
    Sources,
    values_absent_from_latest,
)


class ValuesAbsentFromLatest(unittest.TestCase):
    def test_description_changes_do_not_duplicate_a_field_value(self):
        dictionaries = {
            "FIX.4.3": QuickfixDict(
                "FIX.4.3",
                "FIX43.xml",
                {626: QuickfixField(626, "AllocType", "INT", [("6", "COMBINED")])},
                set(),
            ),
            "FIX.5.0": QuickfixDict(
                "FIX.5.0",
                "FIX50.xml",
                {626: QuickfixField(626, "AllocType", "INT", [("6", "READY")])},
                set(),
            ),
        }
        latest = {
            626: LatestField(
                626,
                "AllocType",
                "int",
                "FIX.4.3",
                0,
                "",
                True,
                [LatestCode("1", "Calculated", "FIX.4.3", 0, "")],
            )
        }

        sources = Sources(
            quickfix=dictionaries,
            session=QuickfixDict("FIXT.1.1", "FIXT11.xml", {}, set()),
            latest=latest,
            latest_source="",
            checksums={},
        )

        values = values_absent_from_latest(sources)

        self.assertEqual(
            values,
            [(626, "6", {"COMBINED": ["FIX.4.3"], "READY": ["FIX.5.0"]})],
        )


if __name__ == "__main__":
    unittest.main()
