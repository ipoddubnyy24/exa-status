"""Tests for seed_commitments.read_dates. Run with the system Python, which
has openpyxl:

    /usr/bin/python3 -m unittest discover -s scripts -p 'test_*.py'
"""

import datetime as dt
import importlib.util
import os
import tempfile
import unittest

import openpyxl

HERE = os.path.dirname(os.path.abspath(__file__))
SPEC = importlib.util.spec_from_file_location("seed_commitments", os.path.join(HERE, "seed_commitments.py"))
seed = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(seed)


def workbook(path, sheets):
    """Writes `sheets` ({title: [rows]}) to an .xlsx file at `path`."""
    book = openpyxl.Workbook()
    book.remove(book.active)
    for title, rows in sheets.items():
        sheet = book.create_sheet(title)
        for row in rows:
            sheet.append(row)
    book.save(path)


class ReadDates(unittest.TestCase):
    def read(self, sheets):
        with tempfile.TemporaryDirectory() as folder:
            path = os.path.join(folder, "tracker.xlsx")
            workbook(path, sheets)
            return seed.read_dates(path)

    def test_only_committed_date_sections_are_read(self):
        found, problems, conflicts = self.read({
            "NVDCSE": [
                ["Ticket", "Summary", "Committed date"],
                ["NVDCSE-247", "Major, idle", dt.datetime(2026, 11, 1)],
                ["Ticket", "Summary", "Resolved date"],
                ["NVDCSE-200", "Closed in September", dt.datetime(2026, 9, 1)],
            ],
            "NCP": [
                ["Ticket", "Notes", "Committed Date (per QBR)"],
                ["NCP-163", "", "2026-11-03"],
            ],
        })

        self.assertEqual(found, {"NVDCSE-247": dt.date(2026, 11, 1), "NCP-163": dt.date(2026, 11, 3)})
        self.assertEqual((problems, conflicts), ([], {}))

    def test_rows_that_cannot_be_promises_are_listed_not_read(self):
        found, problems, _ = self.read({
            "SFDC": [
                ["Ticket", "Summary", "Committed date"],
                ["01342340", "a case number, not a key", dt.datetime(2026, 11, 1)],
                ["SFAP-83257", "no date yet", "TBD"],
                ["Ticket", "Summary", "Owner"],
                ["NVDCSE-1", "a section without dates", dt.datetime(2026, 11, 1)],
            ],
        })

        self.assertEqual(found, {})
        self.assertEqual(problems, [
            "SFDC row 2: '01342340' has a date but is not a Jira key",
            "SFDC row 3: SFAP-83257 has 'TBD', not a date",
        ])

    def test_a_key_with_two_dates_is_a_conflict(self):
        _, _, conflicts = self.read({
            "A": [["Ticket", "Committed date"], ["NVDCSE-247", dt.datetime(2026, 11, 1)]],
            "B": [["Ticket", "Committed date"], ["NVDCSE-247", dt.datetime(2026, 11, 2)]],
        })

        self.assertEqual(conflicts, {"NVDCSE-247": {dt.date(2026, 11, 1), dt.date(2026, 11, 2)}})


if __name__ == "__main__":
    unittest.main()
