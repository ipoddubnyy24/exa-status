"use strict";
// Writes the page's .xlsx exports: one sheet of text cells with a bold,
// frozen header row, as an uncompressed zip. It lives here because the
// page's CSP loads code from the dashboard only. Only `xlsxBlob` is shared
// with app.js; the rest stays inside.

/** An .xlsx workbook with one sheet named `name` holding `rows` of text; the first row is the header. */
const xlsxBlob = (() => {
  const XLSX_TYPE = "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet";
  const XML_HEAD = '<?xml version="1.0" encoding="UTF-8" standalone="yes"?>\n';
  const NS_MAIN = "http://schemas.openxmlformats.org/spreadsheetml/2006/main";
  const NS_DOC_REL = "http://schemas.openxmlformats.org/officeDocument/2006/relationships";
  const NS_PKG_REL = "http://schemas.openxmlformats.org/package/2006/relationships";
  const NS_TYPES = "http://schemas.openxmlformats.org/package/2006/content-types";
  const OFFICE_TYPE = "application/vnd.openxmlformats-officedocument.spreadsheetml";
  /** Widest column, in characters, so a long summary does not fill the screen. */
  const MAX_COLUMN_WIDTH = 60;
  /** Excel's limit on a sheet name. */
  const MAX_SHEET_NAME = 31;
  /** 1 Jan 1980, the earliest date a zip entry can carry; the entries need none. */
  const ZIP_DATE = (1 << 5) | 1;

  /** `text` safe inside XML: escaped, and without the control characters XML forbids. */
  const xmlText = (text) => String(text)
    .replace(/[\u0000-\u0008\u000b\u000c\u000e-\u001f]/g, "")
    .replace(/&/g, "&amp;")
    .replace(/</g, "&lt;")
    .replace(/>/g, "&gt;")
    .replace(/"/g, "&quot;");

  /** "A" for column 0, "Z" for 25, "AA" for 26. */
  function columnName(i) {
    let name = "";
    for (let n = i + 1; n > 0; n = Math.floor((n - 1) / 26)) name = String.fromCharCode(65 + ((n - 1) % 26)) + name;
    return name;
  }

  /** The worksheet: `rows` of text, the first one bold and kept in view. */
  function sheetXml(rows) {
    const widths = [];
    for (const row of rows) row.forEach((cell, i) => { widths[i] = Math.max(widths[i] || 0, String(cell).length); });
    const cols = widths.map((w, i) => `<col min="${i + 1}" max="${i + 1}" width="${Math.min(w, MAX_COLUMN_WIDTH) + 2}" customWidth="1"/>`);
    const cells = rows.map((row, r) => `<row r="${r + 1}">${row.map((cell, c) =>
      `<c r="${columnName(c)}${r + 1}" t="inlineStr"${r ? "" : ' s="1"'}><is><t xml:space="preserve">${xmlText(cell)}</t></is></c>`).join("")}</row>`);
    return `${XML_HEAD}<worksheet xmlns="${NS_MAIN}"><sheetViews><sheetView workbookViewId="0">`
      + '<pane ySplit="1" topLeftCell="A2" activePane="bottomLeft" state="frozen"/></sheetView></sheetViews>'
      + `<cols>${cols.join("")}</cols><sheetData>${cells.join("")}</sheetData></worksheet>`;
  }

  /** The parts of a one-sheet workbook named `name`. */
  function workbookFiles(name, rows) {
    const sheet = name.replace(/[[\]:*?\/\\]/g, " ").slice(0, MAX_SHEET_NAME).trim() || "Sheet1";
    return {
      "[Content_Types].xml": `${XML_HEAD}<Types xmlns="${NS_TYPES}">`
        + '<Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/>'
        + '<Default Extension="xml" ContentType="application/xml"/>'
        + `<Override PartName="/xl/workbook.xml" ContentType="${OFFICE_TYPE}.sheet.main+xml"/>`
        + `<Override PartName="/xl/worksheets/sheet1.xml" ContentType="${OFFICE_TYPE}.worksheet+xml"/>`
        + `<Override PartName="/xl/styles.xml" ContentType="${OFFICE_TYPE}.styles+xml"/></Types>`,
      "_rels/.rels": `${XML_HEAD}<Relationships xmlns="${NS_PKG_REL}">`
        + `<Relationship Id="rId1" Type="${NS_DOC_REL}/officeDocument" Target="xl/workbook.xml"/></Relationships>`,
      "xl/workbook.xml": `${XML_HEAD}<workbook xmlns="${NS_MAIN}" xmlns:r="${NS_DOC_REL}">`
        + `<sheets><sheet name="${xmlText(sheet)}" sheetId="1" r:id="rId1"/></sheets></workbook>`,
      "xl/_rels/workbook.xml.rels": `${XML_HEAD}<Relationships xmlns="${NS_PKG_REL}">`
        + `<Relationship Id="rId1" Type="${NS_DOC_REL}/worksheet" Target="worksheets/sheet1.xml"/>`
        + `<Relationship Id="rId2" Type="${NS_DOC_REL}/styles" Target="styles.xml"/></Relationships>`,
      // Style 1 is the bold header; the two fills are the ones every workbook must have.
      "xl/styles.xml": `${XML_HEAD}<styleSheet xmlns="${NS_MAIN}">`
        + '<fonts count="2"><font><sz val="11"/><name val="Calibri"/></font><font><b/><sz val="11"/><name val="Calibri"/></font></fonts>'
        + '<fills count="2"><fill><patternFill patternType="none"/></fill><fill><patternFill patternType="gray125"/></fill></fills>'
        + '<borders count="1"><border><left/><right/><top/><bottom/><diagonal/></border></borders>'
        + '<cellStyleXfs count="1"><xf numFmtId="0" fontId="0" fillId="0" borderId="0"/></cellStyleXfs>'
        + '<cellXfs count="2"><xf numFmtId="0" fontId="0" fillId="0" borderId="0" xfId="0"/>'
        + '<xf numFmtId="0" fontId="1" fillId="0" borderId="0" xfId="0" applyFont="1"/></cellXfs>'
        + '<cellStyles count="1"><cellStyle name="Normal" xfId="0" builtinId="0"/></cellStyles></styleSheet>',
      "xl/worksheets/sheet1.xml": sheetXml(rows),
    };
  }

  const CRC_TABLE = Array.from({ length: 256 }, (_, n) => {
    let c = n;
    for (let k = 0; k < 8; k++) c = c & 1 ? 0xedb88320 ^ (c >>> 1) : c >>> 1;
    return c >>> 0;
  });

  /** The CRC-32 a zip entry carries. */
  function crc32(bytes) {
    let c = 0xffffffff;
    for (const b of bytes) c = CRC_TABLE[(c ^ b) & 0xff] ^ (c >>> 8);
    return (c ^ 0xffffffff) >>> 0;
  }

  /** Little-endian `[size, value]` fields, as zip headers are written. */
  function fields(list) {
    const out = new Uint8Array(list.reduce((n, [size]) => n + size, 0));
    const view = new DataView(out.buffer);
    let at = 0;
    for (const [size, value] of list) {
      if (size === 2) view.setUint16(at, value, true);
      else view.setUint32(at, value, true);
      at += size;
    }
    return out;
  }

  /** An uncompressed zip of `files` ({ name: text }), as a list of byte parts. */
  function zipParts(files) {
    const encoder = new TextEncoder();
    const local = [];
    const central = [];
    let offset = 0;
    for (const [name, text] of Object.entries(files)) {
      const path = encoder.encode(name);
      const data = encoder.encode(text);
      // Version 2.0, no flags, stored, time 0, the 1980 date, CRC, both sizes, name length.
      const shared = [[2, 20], [2, 0], [2, 0], [2, 0], [2, ZIP_DATE], [4, crc32(data)], [4, data.length], [4, data.length], [2, path.length]];
      local.push(fields([[4, 0x04034b50], ...shared, [2, 0]]), path, data);
      central.push(fields([[4, 0x02014b50], [2, 20], ...shared, [2, 0], [2, 0], [2, 0], [2, 0], [4, 0], [4, offset]]), path);
      offset += 30 + path.length + data.length;
    }
    const size = central.reduce((n, part) => n + part.length, 0);
    const count = Object.keys(files).length;
    const end = fields([[4, 0x06054b50], [2, 0], [2, 0], [2, count], [2, count], [4, size], [4, offset], [2, 0]]);
    return [...local, ...central, end];
  }

  return (name, rows) => new Blob(zipParts(workbookFiles(name, rows)), { type: XLSX_TYPE });
})();
