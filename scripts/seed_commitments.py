#!/usr/bin/env python3
"""Seed commitments.json with the committed dates in an NVIDIA ticket tracker.

Run it with the system Python, which has openpyxl, while the dashboard is
stopped (the page writes the same file):

    /usr/bin/python3 scripts/seed_commitments.py \\
        ~/work/NVIDIA_SFDC_NVDCSE_NCP_Ticket_Status-20261009.xlsx

Each sheet holds sections, each opening with a header row whose first cell is
"Ticket". Only sections whose header has a "Committed date" column (any case
or spacing, extra words after it allowed) are read; a "Resolved / closed"
section has a "Resolved date" column instead and is skipped. Rows whose
first cell is a Jira key give the dates. Entries already in the file are
never replaced, so dates set in the dashboard win. Rows with a date but no
key, and keys with no readable date, are listed; a key given two different
dates stops the seed before anything is written.
"""

import argparse
import datetime as dt
import json
import os
import re
import sys
import tempfile

JIRA_KEY = re.compile(r"^[A-Z][A-Z0-9_]*-[1-9][0-9]*$")
HEADER_FIRST_CELL = "ticket"
DATE_COLUMN = "committed date"


def header(cell):
    """A cell as compared: lowercase, single spaces."""
    return " ".join(str(cell).split()).lower() if cell is not None else ""


def as_date(value):
    """A cell value as a date, or None. Text must be ISO (2026-11-01)."""
    if isinstance(value, dt.datetime):
        return value.date()
    if isinstance(value, dt.date):
        return value
    if isinstance(value, str):
        try:
            return dt.date.fromisoformat(value.strip())
        except ValueError:
            return None
    return None


def read_dates(path):
    """({key: date}, [problems], {key: {dates}} conflicts) from every sheet."""
    import openpyxl  # only the system Python has it; imported here so --help works anywhere

    book = openpyxl.load_workbook(path, data_only=True, read_only=True)
    found, problems, conflicts = {}, [], {}
    for sheet in book.worksheets:
        column = None
        for number, row in enumerate(sheet.iter_rows(values_only=True), start=1):
            labels = [header(c) for c in row]
            if labels and labels[0] == HEADER_FIRST_CELL:
                # Every section header resets the column; only committed-date sections are read.
                dated = [i for i, label in enumerate(labels) if label.startswith(DATE_COLUMN)]
                column = dated[0] if dated else None
                continue
            if column is None or not row:
                continue
            first = str(row[0] or "").strip()
            value = row[column] if column < len(row) else None
            date = as_date(value)
            if not JIRA_KEY.match(first):
                if date is not None:
                    problems.append(f"{sheet.title} row {number}: {first!r} has a date but is not a Jira key")
                continue
            if date is None:
                if value not in (None, ""):
                    problems.append(f"{sheet.title} row {number}: {first} has {value!r}, not a date")
                continue
            if first in found and found[first] != date:
                conflicts.setdefault(first, {found[first]}).add(date)
            found[first] = date
    return found, problems, conflicts


def write_atomically(path, text):
    folder = os.path.dirname(os.path.abspath(path))
    fd, tmp = tempfile.mkstemp(dir=folder, prefix=".commitments-", suffix=".tmp")
    with os.fdopen(fd, "w") as f:
        f.write(text)
    os.replace(tmp, path)


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("tracker", help="the tracker workbook (.xlsx)")
    parser.add_argument("--out", default="commitments.json", help="commitments file (default: %(default)s)")
    parser.add_argument("--source", help="where the dates came from (default: the workbook's file name)")
    args = parser.parse_args()
    source = args.source or os.path.basename(args.tracker)

    dates, problems, conflicts = read_dates(args.tracker)
    for line in problems:
        print(f"not read: {line}")
    if conflicts:
        for key, seen in sorted(conflicts.items()):
            print(f"{key} has different dates: {', '.join(sorted(d.isoformat() for d in seen))}", file=sys.stderr)
        sys.exit("conflicting dates; fix the tracker first, nothing was written")

    current = {}
    if os.path.exists(args.out):
        with open(args.out) as f:
            current = json.load(f)  # an unreadable file stops the seed instead of being replaced
        if not isinstance(current, dict):
            sys.exit(f"{args.out} is not a JSON object; fix it by hand first")

    now = dt.datetime.now().astimezone().isoformat(timespec="seconds")
    added = [k for k in sorted(dates) if k not in current]
    for key in added:
        current[key] = {"date": dates[key].isoformat(), "note": None, "source": source, "updated": now}
    write_atomically(args.out, json.dumps(current, indent=2, sort_keys=True) + "\n")
    print(f"{len(added)} dates added, {len(dates) - len(added)} already present; {len(current)} in {args.out}")


if __name__ == "__main__":
    main()
