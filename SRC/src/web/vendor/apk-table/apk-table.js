// Part of the APK.audio project — http://APK.audio — made by Anthony Kuzub
// MIT Licence. Free, for everyone, for ever. Full text in LICENSE at the root.
/* apk-table.js — THE ONE TABLE. window.APKTable.

   Every table in the project is drawn by this file, so every table answers the
   same script. A page makes one with

     const t = APKTable.create(hostEl, {
       id: 'computer-interfaces',               // its name in the registry
       columns: [
         { key: 'name', label: 'IFace' },                       // editable text
         { key: 'speed', label: 'Speed', type: 'select', options: ['', '1 Gbps'] },
         { key: 'vlan', label: 'VLAN', type: 'number' },
         { key: 'mac', label: 'MAC', editable: false, mono: true },
         { key: 'link', label: 'Link', cellClass: (v) => v ? 'up' : 'down',  // extra td class
           render: (v, row, i) => button('Edit', …) },   // a Node, never an HTML string
       ],
       rowClass(row, i) {},                     // extra tr class
       rows,                                    // array of plain objects
       onEdit(i, key, value, row) {},           // i = index in `rows`, whatever the sort
       onDelete(i, row) {},                     // present → a ✕ column
     });

   and any script — the page's own, the console, an automation — reaches every
   table the same way:

     APKTable.all()                 // [{ id, … }] every live table on the page
     APKTable.get('computer-interfaces')
       .rows() / .setRows(rows) / .sort('vlan', 'desc') / .filter('dante')
       .select(i) / .toCsv() / .exportCsv() / .copy() / .columns()

   A table a page draws ITSELF answers the same calls:

     APKTable.markup({ id, className, head, body, bodyId })   // HTML-string pages:
         // the page's own thead/tbody inner HTML, wrapped; insert it, then
         // APKTable.scan(container) stands the real table in (the document is
         // scanned on load and watched, so static pages need nothing more)
     APKTable.adopt(tableEl, { id })                          // a table already in the DOM
     APKTable.register(id, handle)                            // a renderer of its own (React's
         // OcaTable): it gives what it can of columns/rows/sort/filter/select,
         // the rest (toCsv, exportCsv, copy) is filled in; handle.unregister()

   An adopted table keeps its markup, classes and row handlers; it gains
   header-click sort, filter and select that survive the page rewriting its
   tbody, and its cells (data-sort="…" overrides a cell's text) are its rows().

   Each <table> also carries data-apk-table="<id>", and every edit / delete /
   select is dispatched on it as a bubbling 'apk-table' CustomEvent
   ({ table, action, index, key, value }), so a script can listen to all
   tables at once on document.

   SERVED at /FrontEnd/shared/apk-table.js (+ apk-table.css), the same folder as
   apk-bus.js. A soft app that ships on its own carries a copy in
   vendor/apk-table/, held byte-identical by
   .apk.scripts/vendor_shared_web.py --check.

   Plain script, no framework, no build. Every cell is text (textContent), never
   HTML: rows come off the bus and off files. A column that needs more than text
   (a button, a badge, a swatch) gives render(value, row, i) returning a DOM Node
   it built itself; a string it returns is still text. CSV and Copy take the raw
   value (or format) of such a column, never what render drew. */
(function (root) {
    'use strict';

    var registry = new Map();
    var doc = root.document;

    function text(v) { return v === null || v === undefined ? '' : String(v); }

    function cmp(a, b) {
        var na = Number(a), nb = Number(b);
        if (text(a) !== '' && text(b) !== '' && isFinite(na) && isFinite(nb)) return na - nb;
        return text(a).localeCompare(text(b), undefined, { numeric: true, sensitivity: 'base' });
    }

    function csvCell(v) {
        var s = text(v);
        return /[",\n\r]/.test(s) ? '"' + s.replace(/"/g, '""') + '"' : s;
    }

    function el(tag, cls, txt) {
        var e = doc.createElement(tag);
        if (cls) e.className = cls;
        if (txt !== undefined) e.textContent = txt;
        return e;
    }

    /** Make a table in `host`. Returns its handle (also APKTable.get(id)). */
    function create(host, opts) {
        if (!host) throw new Error('APKTable.create: no host element');
        opts = opts || {};
        var id = opts.id || ('table-' + (registry.size + 1));
        retire(id);

        var columns = (opts.columns || []).map(function (c) {
            return {
                key: c.key,
                label: c.label !== undefined ? c.label : c.key,
                type: c.type || 'text',
                options: c.options || [],
                editable: c.editable !== false && opts.editable !== false && c.type !== 'readonly' && typeof c.render !== 'function',
                mono: !!c.mono,
                width: c.width || '',
                align: c.align || (c.type === 'number' ? 'right' : ''),
                format: c.format || null,
                render: typeof c.render === 'function' ? c.render : null,
                cellClass: typeof c.cellClass === 'function' ? c.cellClass : null,
                placeholder: c.placeholder || '',
            };
        });
        var rows = opts.rows || [];
        var sortKey = null, sortDir = 1, query = '', selected = -1;

        var wrap = el('div', 'apk-table-wrap');
        var bar = el('div', 'apk-table-bar');
        var search = el('input', 'apk-table-search');
        search.type = 'search';
        search.placeholder = opts.searchPlaceholder || 'Filter';
        var count = el('span', 'apk-table-count');
        var csvBtn = el('button', 'apk-table-btn', 'CSV');
        csvBtn.type = 'button';
        csvBtn.title = 'Download this table as CSV';
        var copyBtn = el('button', 'apk-table-btn', 'Copy');
        copyBtn.type = 'button';
        copyBtn.title = 'Copy this table (tab-separated, pastes into a spreadsheet)';
        bar.appendChild(search);
        bar.appendChild(count);
        bar.appendChild(copyBtn);
        bar.appendChild(csvBtn);
        if (opts.toolbar === false) bar.hidden = true;

        var scroller = el('div', 'apk-table-scroll');
        var table = el('table', 'apk-table');
        table.setAttribute('data-apk-table', id);
        var thead = el('thead');
        var tbody = el('tbody');
        table.appendChild(thead);
        table.appendChild(tbody);
        scroller.appendChild(table);
        wrap.appendChild(bar);
        wrap.appendChild(scroller);
        host.innerHTML = '';
        host.appendChild(wrap);

        function emit(action, detail) {
            var d = { table: id, action: action };
            for (var k in detail) d[k] = detail[k];
            try { table.dispatchEvent(new root.CustomEvent('apk-table', { bubbles: true, detail: d })); } catch (e) { /* no CustomEvent */ }
        }

        function visible() {
            var idx = rows.map(function (_, i) { return i; });
            if (query) {
                var q = query.toLowerCase();
                idx = idx.filter(function (i) {
                    return columns.some(function (c) { return text(rows[i][c.key]).toLowerCase().indexOf(q) >= 0; });
                });
            }
            if (sortKey !== null) {
                idx.sort(function (a, b) { return sortDir * cmp(rows[a][sortKey], rows[b][sortKey]) || a - b; });
            }
            return idx;
        }

        function renderHead() {
            thead.innerHTML = '';
            var tr = el('tr');
            columns.forEach(function (c) {
                var th = el('th', c.align ? 'apk-align-' + c.align : '', c.label);
                if (c.width) th.style.width = c.width;
                if (opts.sortable !== false) {
                    th.classList.add('apk-sortable');
                    th.title = 'Sort by ' + c.label;
                    if (sortKey === c.key) th.setAttribute('aria-sort', sortDir > 0 ? 'ascending' : 'descending');
                    th.onclick = function () { handle.sort(c.key, sortKey === c.key && sortDir > 0 ? 'desc' : 'asc'); };
                }
                tr.appendChild(th);
            });
            if (opts.onDelete) tr.appendChild(el('th', 'apk-table-act', ''));
            thead.appendChild(tr);
        }

        function commit(i, c, value) {
            if (text(rows[i][c.key]) === text(value)) return;   // a blur is not an edit
            rows[i][c.key] = c.type === 'number' && value !== '' && isFinite(Number(value)) ? Number(value) : value;
            if (opts.onEdit) opts.onEdit(i, c.key, rows[i][c.key], rows[i]);
            emit('edit', { index: i, key: c.key, value: rows[i][c.key] });
        }

        function cell(i, c) {
            var v = rows[i][c.key];
            var extra = c.cellClass ? text(c.cellClass(v, rows[i], i)) : '';
            var td = el('td', ((c.align ? 'apk-align-' + c.align : '') + (c.mono ? ' apk-mono' : '') + (extra ? ' ' + extra : '')).trim());
            if (c.render) {
                // A Node is appended as built; anything else is text. Never parsed as HTML.
                var out = c.render(v, rows[i], i);
                if (out && typeof out === 'object' && out.nodeType) td.appendChild(out);
                else td.textContent = text(out);
                return td;
            }
            if (c.type === 'select' && c.editable) {
                var sel = el('select', 'apk-table-select');
                var opts2 = c.options.slice();
                if (opts2.indexOf(text(v)) < 0) opts2.push(text(v));
                opts2.forEach(function (o) {
                    var op = el('option', '', o === '' ? '—' : o);
                    op.value = o;
                    if (o === text(v)) op.selected = true;
                    sel.appendChild(op);
                });
                sel.onchange = function () { commit(i, c, sel.value); };
                td.appendChild(sel);
                return td;
            }
            td.textContent = c.format ? c.format(v, rows[i]) : text(v);
            if (c.editable) {
                td.contentEditable = 'true';
                td.classList.add('apk-editable');
                if (c.placeholder) td.setAttribute('data-placeholder', c.placeholder);
                td.onkeydown = function (e) {
                    if (e.key === 'Enter') { e.preventDefault(); td.blur(); }
                    if (e.key === 'Escape') { td.textContent = text(rows[i][c.key]); td.blur(); }
                };
                td.onblur = function () { commit(i, c, td.textContent.trim()); };
            }
            return td;
        }

        function renderBody() {
            tbody.innerHTML = '';
            var idx = visible();
            idx.forEach(function (i) {
                var rc = opts.rowClass ? text(opts.rowClass(rows[i], i)) : '';
                var tr = el('tr', ((i === selected ? 'apk-selected' : '') + (rc ? ' ' + rc : '')).trim());
                tr.setAttribute('data-index', i);
                columns.forEach(function (c) { tr.appendChild(cell(i, c)); });
                if (opts.onDelete) {
                    var td = el('td', 'apk-table-act');
                    var x = el('button', 'apk-table-del', '✕');
                    x.type = 'button';
                    x.title = 'Remove this row';
                    x.onclick = function (e) {
                        e.stopPropagation();
                        var row = rows[i];
                        opts.onDelete(i, row);
                        emit('delete', { index: i });
                    };
                    td.appendChild(x);
                    tr.appendChild(td);
                }
                tr.onclick = function (e) {
                    if (e.target.closest && e.target.closest('[contenteditable="true"], select, button, input, textarea, a')) return;
                    handle.select(i);
                };
                tbody.appendChild(tr);
            });
            if (!idx.length) {
                var tr = el('tr', 'apk-table-empty');
                var td = el('td', '', rows.length ? 'Nothing matches the filter.' : (opts.empty || 'No rows.'));
                td.colSpan = columns.length + (opts.onDelete ? 1 : 0);
                tr.appendChild(td);
                tbody.appendChild(tr);
            }
            count.textContent = idx.length === rows.length ? rows.length + ' rows' : idx.length + ' of ' + rows.length;
        }

        function render() { renderHead(); renderBody(); }

        var handle = {
            id: id,
            el: table,
            columns: function () { return columns.map(function (c) { return { key: c.key, label: c.label, type: c.type }; }); },
            rows: function () { return rows; },
            setRows: function (r) { rows = r || []; if (selected >= rows.length) selected = -1; renderBody(); return handle; },
            sort: function (key, dir) {
                sortKey = key === null || key === undefined ? null : key;
                sortDir = dir === 'desc' ? -1 : 1;
                render();
                return handle;
            },
            filter: function (q) { query = text(q); if (search.value !== query) search.value = query; renderBody(); return handle; },
            select: function (i) {
                selected = i;
                renderBody();
                if (opts.onSelect) opts.onSelect(i, rows[i]);
                emit('select', { index: i });
                return handle;
            },
            /** The table as CSV — the rows shown (filter and sort applied), with a header. */
            toCsv: function (sep) {
                sep = sep || ',';
                var line = function (cells) { return cells.map(sep === ',' ? csvCell : text).join(sep); };
                var out = [line(columns.map(function (c) { return c.label; }))];
                visible().forEach(function (i) {
                    out.push(line(columns.map(function (c) { return c.format ? c.format(rows[i][c.key], rows[i]) : rows[i][c.key]; })));
                });
                return out.join('\n');
            },
            exportCsv: function (filename) {
                var blob = new root.Blob([handle.toCsv()], { type: 'text/csv;charset=utf-8' });
                var a = el('a');
                a.href = root.URL.createObjectURL(blob);
                a.download = filename || (id + '.csv');
                doc.body.appendChild(a);
                a.click();
                a.remove();
                setTimeout(function () { root.URL.revokeObjectURL(a.href); }, 1000);
                return handle;
            },
            copy: function () {
                var tsv = handle.toCsv('\t');
                if (root.navigator && root.navigator.clipboard) return root.navigator.clipboard.writeText(tsv).then(function () { return handle; });
                return Promise.resolve(handle);
            },
            render: function () { render(); return handle; },
            destroy: function () { if (registry.get(id) === handle) registry.delete(id); if (wrap.parentNode) wrap.parentNode.removeChild(wrap); },
        };

        search.oninput = function () { query = search.value; renderBody(); };
        csvBtn.onclick = function () { handle.exportCsv(); };
        copyBtn.onclick = function () {
            handle.copy().then(function () { copyBtn.textContent = 'Copied'; setTimeout(function () { copyBtn.textContent = 'Copy'; }, 1200); });
        };

        registry.set(id, handle);
        render();
        return handle;
    }

    /* An id is one table. A new table under a live id takes the id; the old one
       is destroyed when APKTable drew it, and merely forgotten when it was
       registered (its own renderer owns its DOM). */
    function retire(id, keep) {
        var old = registry.get(id);
        if (!old || old === keep) return;
        registry.delete(id);
        if (old._apkKind !== 'registered' && typeof old.destroy === 'function') old.destroy();
    }

    function slug(s, i) {
        var k = text(s).trim().toLowerCase().replace(/[^a-z0-9]+/g, '-').replace(/^-+|-+$/g, '');
        return k || ('col' + i);
    }

    function cellsOf(tr) {
        var out = [];
        var kids = tr.children || [];
        for (var i = 0; i < kids.length; i++) {
            var t = kids[i].tagName;
            if (t === 'TD' || t === 'TH') out.push(kids[i]);
        }
        return out;
    }

    function childrenTagged(parent, tag) {
        var out = [];
        var kids = (parent && parent.children) || [];
        for (var i = 0; i < kids.length; i++) if (kids[i].tagName === tag) out.push(kids[i]);
        return out;
    }

    function cellValue(td) {
        if (!td) return '';
        var s = td.getAttribute && td.getAttribute('data-sort');
        return s !== null && s !== undefined ? s : text(td.textContent).trim();
    }

    /* Every handle answers the same calls. A renderer that drew its own table
       may give only some of them; the rest are filled here from columns() and
       rows(), so toCsv / exportCsv / copy work on any table. */
    function csvOf(h, sep) {
        sep = sep || ',';
        var cols = h.columns() || [];
        var line = function (cells) { return cells.map(sep === ',' ? csvCell : text).join(sep); };
        var out = [line(cols.map(function (c) { return c.label !== undefined ? c.label : c.key; }))];
        (h.rows() || []).forEach(function (r) {
            out.push(line(cols.map(function (c) { return r ? r[c.key] : ''; })));
        });
        return out.join('\n');
    }

    function fillDefaults(h) {
        var self = function () { return h; };
        if (typeof h.columns !== 'function') h.columns = function () { return []; };
        if (typeof h.rows !== 'function') h.rows = function () { return []; };
        if (typeof h.toCsv !== 'function') h.toCsv = function (sep) { return csvOf(h, sep); };
        if (typeof h.exportCsv !== 'function') {
            h.exportCsv = function (filename) {
                var blob = new root.Blob([h.toCsv()], { type: 'text/csv;charset=utf-8' });
                var a = el('a');
                a.href = root.URL.createObjectURL(blob);
                a.download = filename || (h.id + '.csv');
                doc.body.appendChild(a);
                a.click();
                a.remove();
                setTimeout(function () { root.URL.revokeObjectURL(a.href); }, 1000);
                return h;
            };
        }
        if (typeof h.copy !== 'function') {
            h.copy = function () {
                var tsv = h.toCsv('\t');
                if (root.navigator && root.navigator.clipboard) return root.navigator.clipboard.writeText(tsv).then(self);
                return Promise.resolve(h);
            };
        }
        ['setRows', 'sort', 'filter', 'select', 'render'].forEach(function (k) {
            if (typeof h[k] !== 'function') h[k] = self;   // not offered by this renderer: a no-op, still chainable
        });
        if (typeof h.destroy !== 'function') h.destroy = function () { if (registry.get(h.id) === h) registry.delete(h.id); };
        return h;
    }

    /** A renderer that draws its own table (React's OcaTable…) registers a
     *  handle so scripts reach it like every other. `handle` gives what it can
     *  of columns() / rows() / sort(key, dir) / filter(q) / select(i) /
     *  setRows(r) / el; the rest are filled in. Returns the handle; call
     *  handle.unregister() (or APKTable.unregister(id, handle)) when it unmounts. */
    function register(id, handle) {
        if (!id) throw new Error('APKTable.register: no id');
        if (!handle || typeof handle !== 'object') throw new Error('APKTable.register: no handle');
        handle.id = id;
        handle._apkKind = handle._apkKind || 'registered';
        handle.supports = ['columns', 'rows', 'setRows', 'sort', 'filter', 'select', 'toCsv']
            .filter(function (k) { return typeof handle[k] === 'function'; });
        fillDefaults(handle);
        handle.unregister = function () { if (registry.get(id) === handle) registry.delete(id); };
        retire(id, handle);
        registry.set(id, handle);
        return handle;
    }

    function unregister(id, handle) {
        if (!registry.has(id)) return false;
        if (handle && registry.get(id) !== handle) return false;   // someone newer holds the id
        registry.delete(id);
        return true;
    }

    /** Adopt a <table> whose rows a page writes itself (tbody.innerHTML = …).
     *  The table keeps its own markup, classes and handlers; APKTable gives it
     *  the common handle: header-click sort, filter, select, CSV, copy, and
     *  keeps the sort and filter when the page rewrites its rows. A row whose
     *  only cell spans the table (an empty-state line) is a note, not data. */
    function adopt(table, opts) {
        if (!table) throw new Error('APKTable.adopt: no table');
        opts = opts || {};
        var id = opts.id || table.getAttribute('data-apk-table') || ('table-' + (registry.size + 1));
        retire(id);
        table.setAttribute('data-apk-table', id);
        var thead = childrenTagged(table, 'THEAD')[0] || null;
        var tbody = childrenTagged(table, 'TBODY')[0];
        if (!tbody) { tbody = el('tbody'); table.appendChild(tbody); }
        var sortKey = null, sortDir = 1, query = '', selected = -1, quiet = false, observer = null;
        var bar = opts.bar || null, search = opts.search || null, count = opts.count || null;

        var heads = [], columns = [], sortable = false;

        // The header is read, not declared: a page may write its <thead> after
        // (or long after) the table is adopted, and every rewrite is re-read.
        function readHead() {
            var headRows = thead ? childrenTagged(thead, 'TR') : [];
            heads = headRows.length ? cellsOf(headRows[headRows.length - 1]) : [];
            var spanned = heads.some(function (th) { return (th.colSpan || 1) > 1; });
            var seen = {};
            columns = heads.map(function (th, i) {
                var label = text(th.getAttribute('data-label') || th.textContent).trim();
                var key = th.getAttribute('data-key') || slug(label, i);
                if (seen[key]) key = key + '-' + i;
                seen[key] = true;
                return { key: key, label: label, type: 'text', th: th };
            });
            if (!columns.length) {
                var first = childrenTagged(tbody, 'TR').filter(function (tr) { return cellsOf(tr).length > 1; })[0];
                cellsOf(first || { children: [] }).forEach(function (_, i) { columns.push({ key: 'col' + i, label: '', type: 'text', th: null }); });
            }
            sortable = opts.sortable !== false && heads.length > 0 && !spanned;
            if (sortable) bindHeads();
        }

        function bindHeads() {
            columns.forEach(function (c) {
                if (!c.th || c.th.getAttribute('data-apk-nosort') !== null) return;
                c.th.__apkKey = c.key;
                if (c.th.__apkBound) return;
                c.th.__apkBound = true;
                c.th.classList.add('apk-sortable');
                if (!c.th.title) c.th.title = 'Sort by ' + c.label;
                var th = c.th;
                th.addEventListener('click', function () {
                    var k = th.__apkKey;
                    handle.sort(k, sortKey === k && sortDir > 0 ? 'desc' : 'asc');
                });
            });
        }

        function emit(action, detail) {
            var d = { table: id, action: action };
            for (var k in detail) d[k] = detail[k];
            try { table.dispatchEvent(new root.CustomEvent('apk-table', { bubbles: true, detail: d })); } catch (e) { /* no CustomEvent */ }
        }

        function isNote(tr) {
            var cs = cellsOf(tr);
            if (cs.length === 1 && columns.length > 1) return true;
            return !!(tr.classList && (tr.classList.contains('apk-table-empty') || tr.getAttribute('data-apk-note') !== null));
        }

        // A row marked data-apk-detail (an expanded flow's channels…) belongs to
        // the data row above it: it sorts, filters and moves with that row.
        function isDetail(tr) { return tr.getAttribute('data-apk-detail') !== null; }
        function allRows() { return childrenTagged(tbody, 'TR'); }
        function dataRows() { return allRows().filter(function (tr) { return !isNote(tr) && !isDetail(tr); }); }
        function link() {
            var last = null;
            allRows().forEach(function (tr) {
                if (isDetail(tr)) { if (last) last.__apkDetails.push(tr); }
                else if (!isNote(tr)) { last = tr; tr.__apkDetails = []; }
            });
        }
        function detailsOf(tr) { return tr.__apkDetails || []; }
        function order(tr) { return Number(tr.getAttribute('data-apk-order')); }

        // Rows arrive in the page's order; that order is index i for select()
        // and rows(), whatever the sort shows.
        function stamp() {
            link();
            var rs = dataRows();
            var fresh = rs.filter(function (tr) { return tr.getAttribute('data-apk-order') === null; });
            if (fresh.length === rs.length) {
                // a rewrite: the page's order is the DOM's
                rs.forEach(function (tr, i) { tr.setAttribute('data-apk-order', i); });
                return;
            }
            // rows added to kept ones: they come after them, in the order added
            var top = -1;
            rs.forEach(function (tr) { if (tr.getAttribute('data-apk-order') !== null) top = Math.max(top, order(tr)); });
            fresh.forEach(function (tr) { tr.setAttribute('data-apk-order', ++top); });
        }

        function ordered() {
            return dataRows().slice().sort(function (a, b) { return order(a) - order(b); });
        }

        function rowObject(tr) {
            var o = {};
            var cs = cellsOf(tr);
            columns.forEach(function (c, i) { o[c.key] = cellValue(cs[i]); });
            return o;
        }

        function matchesQuery(tr) {
            if (!query) return true;
            return text(tr.textContent).toLowerCase().indexOf(query.toLowerCase()) >= 0;
        }

        function apply() {
            quiet = true;
            var rs = ordered();
            if (sortKey !== null) {
                var ci = -1;
                columns.forEach(function (c, i) { if (c.key === sortKey) ci = i; });
                if (ci >= 0) {
                    rs.sort(function (a, b) {
                        return sortDir * cmp(cellValue(cellsOf(a)[ci]), cellValue(cellsOf(b)[ci])) || order(a) - order(b);
                    });
                }
            }
            if (sortKey !== null || query) {
                // move data rows into the shown order; notes stay where they were
                rs.forEach(function (tr) {
                    tbody.appendChild(tr);
                    detailsOf(tr).forEach(function (d) { tbody.appendChild(d); });
                });
            }
            var shown = 0;
            rs.forEach(function (tr) {
                var on = matchesQuery(tr);
                if (on) shown++;
                tr.style.display = on ? '' : 'none';
                detailsOf(tr).forEach(function (d) { d.style.display = on ? '' : 'none'; });
                if (tr.classList) tr.classList.toggle('apk-selected', order(tr) === selected);
            });
            if (count) count.textContent = shown === rs.length ? rs.length + ' rows' : shown + ' of ' + rs.length;
            heads.forEach(function (th) { th.removeAttribute('aria-sort'); });
            columns.forEach(function (c) {
                if (c.th && c.key === sortKey) c.th.setAttribute('aria-sort', sortDir > 0 ? 'ascending' : 'descending');
            });
            if (observer && observer.takeRecords) observer.takeRecords();
            quiet = false;
        }

        function refresh() { if (!columns.length) readHead(); stamp(); apply(); }

        var handle = {
            id: id,
            el: table,
            body: tbody,
            _apkKind: 'adopted',
            columns: function () { return columns.map(function (c) { return { key: c.key, label: c.label, type: c.type }; }); },
            rows: function () { return ordered().map(rowObject); },
            /** Replace the rows with plain objects, drawn as text. */
            setRows: function (r) {
                tbody.innerHTML = '';
                (r || []).forEach(function (row) {
                    var tr = el('tr');
                    columns.forEach(function (c) { tr.appendChild(el('td', '', text(row && row[c.key]))); });
                    tbody.appendChild(tr);
                });
                refresh();
                return handle;
            },
            sort: function (key, dir) {
                sortKey = key === null || key === undefined ? null : key;
                sortDir = dir === 'desc' ? -1 : 1;
                if (sortKey === null) {
                    quiet = true;
                    ordered().forEach(function (tr) {
                        tbody.appendChild(tr);
                        detailsOf(tr).forEach(function (d) { tbody.appendChild(d); });
                    });
                    quiet = false;
                }
                apply();
                emit('sort', { key: sortKey, value: sortDir > 0 ? 'asc' : 'desc' });
                return handle;
            },
            filter: function (q) { query = text(q); if (search && search.value !== query) search.value = query; apply(); return handle; },
            select: function (i) {
                selected = i;
                apply();
                if (opts.onSelect) opts.onSelect(i, handle.rows()[i]);
                emit('select', { index: i });
                return handle;
            },
            toCsv: function (sep) {
                sep = sep || ',';
                var line = function (cells) { return cells.map(sep === ',' ? csvCell : text).join(sep); };
                var out = [line(columns.map(function (c) { return c.label; }))];
                dataRows().forEach(function (tr) {
                    if (tr.style.display === 'none') return;
                    var cs = cellsOf(tr);
                    out.push(line(columns.map(function (_, i) { return cellValue(cs[i]); })));
                });
                return out.join('\n');
            },
            /** Call after rewriting rows when no MutationObserver is around. */
            render: function () { refresh(); return handle; },
            destroy: function () {
                if (observer) observer.disconnect();
                if (registry.get(id) === handle) registry.delete(id);
            },
        };
        fillDefaults(handle);

        if (search) search.oninput = function () { query = search.value; apply(); };
        if (bar) {
            var btns = bar.querySelectorAll('.apk-table-btn');
            if (btns[0]) btns[0].onclick = function () { handle.copy(); };
            if (btns[1]) btns[1].onclick = function () { handle.exportCsv(); };
        }

        // The page owns its rows and rewrites them at will; the sort and the
        // filter survive the rewrite.
        if (root.MutationObserver) {
            observer = new root.MutationObserver(function () { if (!quiet) refresh(); });
            observer.observe(tbody, { childList: true });
            if (thead) new root.MutationObserver(function () { readHead(); apply(); }).observe(thead, { childList: true, subtree: true });
        }

        registry.set(id, handle);
        readHead();
        refresh();
        return handle;
    }

    function attr(v) {
        return text(v).replace(/&/g, '&amp;').replace(/"/g, '&quot;').replace(/</g, '&lt;').replace(/>/g, '&gt;');
    }

    var PARTS = ['caption', 'colgroup', 'head', 'body', 'foot'];

    /** The markup of an adoptable table, for a page that builds its HTML as a
     *  string. `head` / `body` / `foot` / `caption` / `colgroup` are that part's
     *  inner HTML, exactly as the page would have written it (escaping its
     *  values is still the page's job). Insert it, then APKTable.scan(container)
     *  (or let the observer find it) and the real table stands in its place.
     *  opts: id, className, style, tableId, headId, bodyId, bodyClass, sortable,
     *  toolbar. */
    function markup(opts) {
        opts = opts || {};
        var o = {};
        ['id', 'className', 'style', 'tableId', 'headId', 'bodyId', 'bodyClass', 'sortable', 'toolbar', 'searchPlaceholder'].forEach(function (k) {
            if (opts[k] !== undefined) o[k] = opts[k];
        });
        var s = '<div class="apk-table-host" data-apk-table-host="' + attr(o.id || '') + '" data-apk-opts="' + attr(JSON.stringify(o)) + '">';
        PARTS.forEach(function (p) {
            if (opts[p] !== undefined && opts[p] !== null) s += '<template data-apk-part="' + p + '">' + opts[p] + '</template>';
        });
        return s + '</div>';
    }

    function bar(opts) {
        var b = el('div', 'apk-table-bar');
        var search = el('input', 'apk-table-search');
        search.type = 'search';
        search.placeholder = opts.searchPlaceholder || 'Filter';
        var count = el('span', 'apk-table-count');
        var copyBtn = el('button', 'apk-table-btn', 'Copy');
        copyBtn.type = 'button';
        var csvBtn = el('button', 'apk-table-btn', 'CSV');
        csvBtn.type = 'button';
        b.appendChild(search); b.appendChild(count); b.appendChild(copyBtn); b.appendChild(csvBtn);
        return { bar: b, search: search, count: count };
    }

    function moveContent(tpl, into) {
        var src = tpl.content || tpl;   // a <template>'s fragment
        while (src.firstChild) into.appendChild(src.firstChild);
    }

    /** Stand the real table in for one markup() host. Returns its handle. */
    function upgrade(host) {
        var o = {};
        try { o = JSON.parse(host.getAttribute('data-apk-opts') || '{}') || {}; } catch (e) { o = {}; }
        if (!o.id) o.id = host.getAttribute('data-apk-table-host') || undefined;
        var table = el('table', o.className || '');
        if (o.style) table.setAttribute('style', o.style);
        if (o.tableId) table.setAttribute('id', o.tableId);
        var parts = {};
        childrenTagged(host, 'TEMPLATE').forEach(function (t) { parts[t.getAttribute('data-apk-part')] = t; });
        var tags = { caption: 'caption', colgroup: 'colgroup', head: 'thead', body: 'tbody', foot: 'tfoot' };
        PARTS.forEach(function (p) {
            if (!parts[p] && p !== 'body') return;
            var part = el(tags[p]);
            if (p === 'body' && o.bodyId) part.setAttribute('id', o.bodyId);
            if (p === 'head' && o.headId) part.setAttribute('id', o.headId);
            if (p === 'body' && o.bodyClass) part.className = o.bodyClass;
            if (parts[p]) moveContent(parts[p], part);
            table.appendChild(part);
        });
        var parent = host.parentNode;
        var extra = {};
        scan(table);   // a table drawn inside one of its cells
        if (o.toolbar === true) {
            extra = bar(o);
            if (parent) parent.insertBefore(extra.bar, host);
        }
        if (parent) parent.replaceChild(table, host);
        o.bar = extra.bar; o.search = extra.search; o.count = extra.count;
        return adopt(table, o);
    }

    /** Upgrade every markup() host in `scope` (default: the document). */
    function scan(scope) {
        scope = scope || doc;
        if (!scope || !scope.querySelectorAll) return [];
        var hosts = [];
        if (scope.getAttribute && scope.getAttribute('data-apk-table-host') !== null) hosts.push(scope);
        var found = scope.querySelectorAll('[data-apk-table-host]');
        for (var i = 0; i < found.length; i++) hosts.push(found[i]);
        return hosts.map(upgrade);
    }

    function all() {
        var out = [];
        registry.forEach(function (h, id) {
            // a table the page threw away (its container rewritten) is gone
            if (h._apkKind === 'adopted' && h.el && h.el.isConnected === false && h._seen) { registry.delete(id); return; }
            if (h.el && h.el.isConnected) h._seen = true;
            out.push(h);
        });
        return out;
    }

    var api = {
        create: create,
        adopt: adopt,
        register: register,
        unregister: unregister,
        markup: markup,
        scan: scan,
        get: function (id) { return registry.get(id) || null; },
        all: all,
    };

    // Static pages: the hosts already parsed when this script runs, and the rest
    // once the document is in. Pages that write markup() later get it upgraded
    // by the observer (or by calling APKTable.scan(container) right away, which
    // they must when they query the rows in the same tick).
    if (doc && doc.querySelectorAll) {
        scan(doc);
        if (doc.addEventListener) doc.addEventListener('DOMContentLoaded', function () { scan(doc); });
        if (root.MutationObserver && doc.documentElement) {
            new root.MutationObserver(function (records) {
                records.forEach(function (r) {
                    for (var i = 0; i < r.addedNodes.length; i++) {
                        var n = r.addedNodes[i];
                        if (n.nodeType === 1 && n.isConnected !== false) scan(n);
                    }
                });
            }).observe(doc.documentElement, { childList: true, subtree: true });
        }
    }
    root.APKTable = api;
    if (typeof module === 'object' && module.exports) module.exports = api;
})(typeof window !== 'undefined' ? window : globalThis);
