import Cocoa
import WebKit

class OverlayWindow: NSWindow {
    override var canBecomeKey: Bool { true }
    override var canBecomeMain: Bool { true }
}

class OverlayController: NSObject, WKNavigationDelegate, NSWindowDelegate {
    let window: OverlayWindow
    let webView: WKWebView
    var filePath: String
    var initialOpacity: CGFloat
    var fileContent: String = ""

    init(filePath: String, x: Int, y: Int, w: Int, h: Int, opacity: Double) {
        self.filePath = filePath
        self.initialOpacity = CGFloat(opacity)

        let rect = NSRect(x: x, y: y, width: w, height: h)
        window = OverlayWindow(
            contentRect: rect,
            styleMask: [.titled, .closable, .resizable, .fullSizeContentView],
            backing: .buffered,
            defer: false
        )

        let config = WKWebViewConfiguration()
        config.preferences.setValue(true, forKey: "developerExtrasEnabled")
        webView = WKWebView(frame: .zero, configuration: config)
        webView.translatesAutoresizingMaskIntoConstraints = false

        super.init()

        window.delegate = self
        window.level = .floating
        window.isOpaque = false
        window.backgroundColor = .clear
        window.titlebarAppearsTransparent = true
        window.titleVisibility = .hidden
        window.isMovableByWindowBackground = true
        window.alphaValue = initialOpacity
        window.hasShadow = true
        window.minSize = NSSize(width: 250, height: 200)

        let visual = NSVisualEffectView()
        visual.material = .hudWindow
        visual.blendingMode = .behindWindow
        visual.state = .active
        visual.translatesAutoresizingMaskIntoConstraints = false

        window.contentView = visual
        visual.addSubview(webView)

        NSLayoutConstraint.activate([
            webView.topAnchor.constraint(equalTo: visual.topAnchor),
            webView.bottomAnchor.constraint(equalTo: visual.bottomAnchor),
            webView.leadingAnchor.constraint(equalTo: visual.leadingAnchor),
            webView.trailingAnchor.constraint(equalTo: visual.trailingAnchor),
        ])

        webView.navigationDelegate = self
        webView.setValue(false, forKey: "drawsBackground")

        let opacityHandler = OpacityHandler(window: window)
        let toggleHandler = ToggleHandler(controller: self)
        let editHandler = EditHandler(controller: self)
        webView.configuration.userContentController.add(opacityHandler, name: "opacity")
        webView.configuration.userContentController.add(toggleHandler, name: "toggle")
        webView.configuration.userContentController.add(editHandler, name: "edit")

        loadContent()
        window.makeKeyAndOrderFront(nil)
    }

    func loadContent() {
        let url = URL(fileURLWithPath: filePath)
        fileContent = (try? String(contentsOf: url, encoding: .utf8)) ?? "Could not read file."

        let renderedBody = renderMarkdown(fileContent)
        let opPct = Int(initialOpacity * 100)

        let html = """
        <!DOCTYPE html>
        <html>
        <head>
        <meta charset="utf-8">
        <style>
            :root {
                --fg: #e8e8e8;
                --fg-dim: rgba(255,255,255,0.45);
                --fg-muted: rgba(255,255,255,0.5);
                --fg-heading: rgba(255,255,255,0.9);
                --accent: #5ba3f5;
                --code-bg: rgba(0,0,0,0.25);
                --border: rgba(255,255,255,0.15);
                --table-header: rgba(255,255,255,0.05);
                --toolbar-bg: rgba(255,255,255,0.05);
                --toolbar-hover: rgba(255,255,255,0.1);
                --toolbar-fg: rgba(255,255,255,0.6);
                --scrollbar: rgba(255,255,255,0.15);
            }
            [data-theme="light"] {
                --fg: #1a1a1a;
                --fg-dim: rgba(0,0,0,0.4);
                --fg-muted: rgba(0,0,0,0.45);
                --fg-heading: rgba(0,0,0,0.85);
                --accent: #2563eb;
                --code-bg: rgba(0,0,0,0.06);
                --border: rgba(0,0,0,0.12);
                --table-header: rgba(0,0,0,0.04);
                --toolbar-bg: rgba(0,0,0,0.04);
                --toolbar-hover: rgba(0,0,0,0.08);
                --toolbar-fg: rgba(0,0,0,0.5);
                --scrollbar: rgba(0,0,0,0.12);
            }
            [data-theme="warm"] {
                --fg: #f5e6d3;
                --fg-dim: rgba(245,230,211,0.45);
                --fg-muted: rgba(245,230,211,0.5);
                --fg-heading: rgba(245,230,211,0.9);
                --accent: #e8a64c;
                --code-bg: rgba(0,0,0,0.2);
                --border: rgba(245,230,211,0.15);
                --table-header: rgba(245,230,211,0.05);
                --toolbar-bg: rgba(245,230,211,0.05);
                --toolbar-hover: rgba(245,230,211,0.1);
                --toolbar-fg: rgba(245,230,211,0.5);
                --scrollbar: rgba(245,230,211,0.15);
            }
            [data-theme="ocean"] {
                --fg: #c8e6f5;
                --fg-dim: rgba(200,230,245,0.45);
                --fg-muted: rgba(200,230,245,0.5);
                --fg-heading: rgba(200,230,245,0.9);
                --accent: #38bdf8;
                --code-bg: rgba(0,20,40,0.3);
                --border: rgba(200,230,245,0.15);
                --table-header: rgba(200,230,245,0.05);
                --toolbar-bg: rgba(200,230,245,0.05);
                --toolbar-hover: rgba(200,230,245,0.1);
                --toolbar-fg: rgba(200,230,245,0.5);
                --scrollbar: rgba(200,230,245,0.15);
            }
            [data-theme="rose"] {
                --fg: #f5d5e0;
                --fg-dim: rgba(245,213,224,0.45);
                --fg-muted: rgba(245,213,224,0.5);
                --fg-heading: rgba(245,213,224,0.9);
                --accent: #f472b6;
                --code-bg: rgba(30,0,15,0.25);
                --border: rgba(245,213,224,0.15);
                --table-header: rgba(245,213,224,0.05);
                --toolbar-bg: rgba(245,213,224,0.05);
                --toolbar-hover: rgba(245,213,224,0.1);
                --toolbar-fg: rgba(245,213,224,0.5);
                --scrollbar: rgba(245,213,224,0.15);
            }
            [data-theme="green"] {
                --fg: #d4edda;
                --fg-dim: rgba(212,237,218,0.45);
                --fg-muted: rgba(212,237,218,0.5);
                --fg-heading: rgba(212,237,218,0.9);
                --accent: #34d399;
                --code-bg: rgba(0,20,10,0.25);
                --border: rgba(212,237,218,0.15);
                --table-header: rgba(212,237,218,0.05);
                --toolbar-bg: rgba(212,237,218,0.05);
                --toolbar-hover: rgba(212,237,218,0.1);
                --toolbar-fg: rgba(212,237,218,0.5);
                --scrollbar: rgba(212,237,218,0.15);
            }

            * { margin: 0; padding: 0; box-sizing: border-box; }
            html, body { height: 100%; overflow-y: auto; }
            body {
                font-family: -apple-system, BlinkMacSystemFont, "SF Pro Text", sans-serif;
                font-size: 14px;
                color: var(--fg);
                padding: 16px 20px 20px;
                line-height: 1.55;
            }
            .header {
                display: flex;
                align-items: center;
                justify-content: space-between;
                margin-bottom: 14px;
            }
            .pin-icon { font-size: 16px; opacity: 0.7; cursor: default; }
            .controls {
                display: flex; align-items: center; gap: 6px;
            }
            .opacity-row {
                display: flex; align-items: center; gap: 6px;
                font-size: 12px; color: var(--fg-muted);
            }
            .opacity-row input[type=range] { width: 80px; accent-color: var(--accent); }

            h1 { font-size: 22px; font-weight: 700; margin-bottom: 2px; }
            h2 { font-size: 16px; font-weight: 700; margin: 18px 0 8px; color: var(--fg-heading); }
            h3 { font-size: 14px; font-weight: 700; margin: 14px 0 6px; color: var(--fg-heading); }
            h4 { font-size: 13px; font-weight: 600; margin: 12px 0 4px; }
            .date { font-size: 13px; color: var(--fg-dim); margin-bottom: 16px; }

            p { margin: 6px 0; }
            strong { font-weight: 700; }
            em { font-style: italic; }

            a { color: var(--accent); text-decoration: none; }
            a:hover { text-decoration: underline; }

            ul, ol { padding-left: 20px; margin: 4px 0; }
            ul { list-style: disc; }
            ol { list-style: decimal; }
            li { padding: 2px 0; }

            .task-item {
                display: flex; align-items: flex-start; gap: 8px;
                padding: 4px 0; list-style: none;
            }
            .task-item input[type=checkbox] {
                margin-top: 3px; accent-color: var(--accent); cursor: pointer;
                width: 16px; height: 16px;
            }
            .task-item.checked > span { text-decoration: line-through; opacity: 0.5; }

            blockquote {
                border-left: 3px solid var(--border);
                padding: 4px 0 4px 14px; margin: 8px 0;
                color: var(--fg-muted); font-style: italic;
            }

            code {
                background: var(--code-bg); padding: 1px 5px;
                border-radius: 3px; font-size: 12px; font-family: "SF Mono", Menlo, monospace;
            }
            pre {
                background: var(--code-bg); padding: 10px 12px;
                border-radius: 6px; overflow-x: auto; margin: 8px 0;
                font-size: 12px; line-height: 1.4;
            }
            pre code { background: none; padding: 0; }

            hr {
                border: none; border-top: 1px solid var(--border);
                margin: 12px 0;
            }

            table { border-collapse: collapse; margin: 8px 0; width: 100%; font-size: 13px; }
            th, td { border: 1px solid var(--border); padding: 5px 8px; text-align: left; }
            th { background: var(--table-header); font-weight: 600; }

            img { max-width: 100%; border-radius: 4px; margin: 4px 0; }

            .edit-area {
                width: 100%; min-height: 120px; background: var(--code-bg);
                border: 1px solid var(--border); border-radius: 6px;
                color: var(--fg); font-family: "SF Mono", Menlo, monospace;
                font-size: 13px; padding: 10px; resize: vertical; line-height: 1.5;
                display: none;
            }
            .edit-area:focus { outline: 1px solid var(--accent); }
            .edit-bar { display: none; gap: 6px; margin-top: 6px; }
            .edit-bar button {
                font-size: 12px; padding: 4px 10px; border-radius: 4px;
                border: 1px solid var(--border); background: var(--toolbar-bg);
                color: var(--fg); cursor: pointer;
            }
            .edit-bar button.save { background: var(--accent); border-color: var(--accent); color: #fff; }
            .edit-bar button:hover { opacity: 0.85; }

            .toolbar-btn {
                font-size: 11px; padding: 3px 8px; border-radius: 4px;
                border: 1px solid var(--border); background: var(--toolbar-bg);
                color: var(--toolbar-fg); cursor: pointer;
            }
            .toolbar-btn:hover { background: var(--toolbar-hover); color: var(--fg); }

            .theme-menu {
                position: absolute; right: 0; top: 24px;
                background: rgba(30,30,30,0.95); border: 1px solid var(--border);
                border-radius: 8px; padding: 4px; display: none; z-index: 10;
                min-width: 110px; backdrop-filter: blur(10px);
            }
            .theme-menu.open { display: block; }
            .theme-opt {
                display: flex; align-items: center; gap: 8px;
                padding: 5px 10px; border-radius: 5px; cursor: pointer;
                font-size: 12px; color: #ccc; white-space: nowrap;
            }
            .theme-opt:hover { background: rgba(255,255,255,0.1); }
            .theme-opt.active { color: #fff; font-weight: 600; }
            .theme-swatch {
                width: 12px; height: 12px; border-radius: 50%;
                border: 1px solid rgba(255,255,255,0.2);
            }

            ::-webkit-scrollbar { width: 6px; }
            ::-webkit-scrollbar-track { background: transparent; }
            ::-webkit-scrollbar-thumb { background: var(--scrollbar); border-radius: 3px; }
        </style>
        </head>
        <body>
            <div class="header">
                <span class="pin-icon">📌</span>
                <div class="controls">
                    <button class="toolbar-btn" id="editBtn" onclick="toggleEdit()">✎ Edit</button>
                    <div style="position:relative;">
                        <button class="toolbar-btn" id="themeBtn" onclick="toggleThemeMenu()">🎨</button>
                        <div class="theme-menu" id="themeMenu">
                            <div class="theme-opt" onclick="setTheme('dark')"><span class="theme-swatch" style="background:#1a1a2e;"></span>Dark</div>
                            <div class="theme-opt" onclick="setTheme('light')"><span class="theme-swatch" style="background:#f5f5f5;"></span>Light</div>
                            <div class="theme-opt" onclick="setTheme('warm')"><span class="theme-swatch" style="background:#4a3728;"></span>Warm</div>
                            <div class="theme-opt" onclick="setTheme('ocean')"><span class="theme-swatch" style="background:#0c2d48;"></span>Ocean</div>
                            <div class="theme-opt" onclick="setTheme('rose')"><span class="theme-swatch" style="background:#3d1f2e;"></span>Rose</div>
                            <div class="theme-opt" onclick="setTheme('green')"><span class="theme-swatch" style="background:#1a3d2a;"></span>Green</div>
                        </div>
                    </div>
                    <div class="opacity-row">
                        <input type="range" id="opSlider" min="20" max="100" value="\(opPct)"
                               oninput="setOpacity(this.value)">
                        <span id="opLabel">\(opPct)%</span>
                    </div>
                </div>
            </div>
            <div id="rendered">\(renderedBody)</div>
            <textarea class="edit-area" id="editArea"></textarea>
            <div class="edit-bar" id="editBar">
                <button class="save" onclick="saveEdit()">Save</button>
                <button onclick="cancelEdit()">Cancel</button>
            </div>
            <script>
                function setOpacity(val) {
                    document.getElementById('opLabel').textContent = val + '%';
                    window.webkit.messageHandlers.opacity.postMessage(val);
                }

                function toggleThemeMenu() {
                    document.getElementById('themeMenu').classList.toggle('open');
                }
                document.addEventListener('click', function(e) {
                    if (!e.target.closest('#themeBtn') && !e.target.closest('#themeMenu'))
                        document.getElementById('themeMenu').classList.remove('open');
                });

                function setTheme(name) {
                    document.documentElement.setAttribute('data-theme', name === 'dark' ? '' : name);
                    if (name === 'dark') document.documentElement.removeAttribute('data-theme');
                    try { localStorage.setItem('pinned_theme', name); } catch(e) {}
                    document.querySelectorAll('.theme-opt').forEach(function(el) {
                        el.classList.toggle('active', el.textContent.trim().toLowerCase() === name);
                    });
                    document.getElementById('themeMenu').classList.remove('open');
                }
                (function() {
                    var saved = 'dark';
                    try { saved = localStorage.getItem('pinned_theme') || 'dark'; } catch(e) {}
                    setTheme(saved);
                })();

                function handleToggle(lineIdx, checked) {
                    window.webkit.messageHandlers.toggle.postMessage(
                        JSON.stringify({ line: lineIdx, checked: checked })
                    );
                }

                let _editMode = false;
                function toggleEdit() {
                    _editMode = !_editMode;
                    const rendered = document.getElementById('rendered');
                    const area = document.getElementById('editArea');
                    const bar = document.getElementById('editBar');
                    const btn = document.getElementById('editBtn');
                    if (_editMode) {
                        window.webkit.messageHandlers.edit.postMessage('get');
                    } else {
                        cancelEdit();
                    }
                }

                function showEditMode(content) {
                    const rendered = document.getElementById('rendered');
                    const area = document.getElementById('editArea');
                    const bar = document.getElementById('editBar');
                    rendered.style.display = 'none';
                    area.style.display = '';
                    area.value = content;
                    bar.style.display = 'flex';
                    document.getElementById('editBtn').textContent = '✕ Cancel';
                    area.focus();
                }

                function saveEdit() {
                    const content = document.getElementById('editArea').value;
                    window.webkit.messageHandlers.edit.postMessage('save:' + content);
                }

                function cancelEdit() {
                    _editMode = false;
                    document.getElementById('rendered').style.display = '';
                    document.getElementById('editArea').style.display = 'none';
                    document.getElementById('editBar').style.display = 'none';
                    document.getElementById('editBtn').textContent = '✎ Edit';
                }
            </script>
        </body>
        </html>
        """

        webView.loadHTMLString(html, baseURL: URL(fileURLWithPath: filePath).deletingLastPathComponent())
    }

    func toggleCheckbox(lineIndex: Int, checked: Bool) {
        var lines = fileContent.components(separatedBy: "\n")
        guard lineIndex >= 0 && lineIndex < lines.count else { return }

        let line = lines[lineIndex]
        if checked {
            lines[lineIndex] = line
                .replacingOccurrences(of: "- [ ] ", with: "- [x] ")
                .replacingOccurrences(of: "- [ ]", with: "- [x]")
        } else {
            lines[lineIndex] = line
                .replacingOccurrences(of: "- [x] ", with: "- [ ] ")
                .replacingOccurrences(of: "- [X] ", with: "- [ ] ")
                .replacingOccurrences(of: "- [x]", with: "- [ ]")
                .replacingOccurrences(of: "- [X]", with: "- [ ]")
        }

        fileContent = lines.joined(separator: "\n")
        try? fileContent.write(toFile: filePath, atomically: true, encoding: .utf8)
        loadContent()
    }

    func saveContent(_ newContent: String) {
        fileContent = newContent
        try? fileContent.write(toFile: filePath, atomically: true, encoding: .utf8)
        loadContent()
    }

    func renderMarkdown(_ text: String) -> String {
        var html = ""
        let lines = text.components(separatedBy: "\n")
        var inCodeBlock = false
        var codeBlockContent = ""
        var inTable = false
        var tableRows: [[String]] = []
        var inList = false
        var listType = ""

        let dateFormatter = DateFormatter()
        dateFormatter.dateFormat = "EEE, MMM d, yyyy"
        let dateStr = dateFormatter.string(from: Date())
        var titleRendered = false

        for (idx, rawLine) in lines.enumerated() {
            let line = rawLine

            if line.hasPrefix("```") {
                if inCodeBlock {
                    html += "<pre><code>" + escapeHTML(codeBlockContent) + "</code></pre>"
                    codeBlockContent = ""
                    inCodeBlock = false
                } else {
                    if inList { html += listType == "ol" ? "</ol>" : "</ul>"; inList = false }
                    if inTable { html += renderTable(tableRows); inTable = false; tableRows = [] }
                    inCodeBlock = true
                }
                continue
            }

            if inCodeBlock {
                if !codeBlockContent.isEmpty { codeBlockContent += "\n" }
                codeBlockContent += line
                continue
            }

            let trimmed = line.trimmingCharacters(in: .whitespaces)

            if trimmed.contains("|") && trimmed.hasPrefix("|") {
                if inList { html += listType == "ol" ? "</ol>" : "</ul>"; inList = false }
                let cells = trimmed.split(separator: "|", omittingEmptySubsequences: false)
                    .map { $0.trimmingCharacters(in: .whitespaces) }
                    .filter { !$0.isEmpty }
                let isSeparator = cells.allSatisfy { $0.allSatisfy { $0 == "-" || $0 == ":" || $0 == " " } }
                if isSeparator { continue }
                if !inTable { inTable = true; tableRows = [] }
                tableRows.append(cells)
                continue
            } else if inTable {
                html += renderTable(tableRows)
                inTable = false
                tableRows = []
            }

            if trimmed.isEmpty {
                if inList { html += listType == "ol" ? "</ol>" : "</ul>"; inList = false }
                continue
            }

            if trimmed.hasPrefix("# ") {
                if inList { html += listType == "ol" ? "</ol>" : "</ul>"; inList = false }
                let heading = String(trimmed.dropFirst(2))
                html += "<h1>" + inlineMarkdown(heading) + "</h1>"
                if !titleRendered {
                    html += "<div class=\"date\">" + dateStr + "</div>"
                    titleRendered = true
                }
            } else if trimmed.hasPrefix("## ") {
                if inList { html += listType == "ol" ? "</ol>" : "</ul>"; inList = false }
                html += "<h2>" + inlineMarkdown(String(trimmed.dropFirst(3))) + "</h2>"
            } else if trimmed.hasPrefix("### ") {
                if inList { html += listType == "ol" ? "</ol>" : "</ul>"; inList = false }
                html += "<h3>" + inlineMarkdown(String(trimmed.dropFirst(4))) + "</h3>"
            } else if trimmed.hasPrefix("#### ") {
                if inList { html += listType == "ol" ? "</ol>" : "</ul>"; inList = false }
                html += "<h4>" + inlineMarkdown(String(trimmed.dropFirst(5))) + "</h4>"
            } else if trimmed == "---" || trimmed == "***" || trimmed == "___" {
                if inList { html += listType == "ol" ? "</ol>" : "</ul>"; inList = false }
                html += "<hr>"
            } else if trimmed.hasPrefix("- [ ] ") || trimmed.hasPrefix("- [x] ") || trimmed.hasPrefix("- [X] ") {
                if !inList || listType != "ul" {
                    if inList { html += listType == "ol" ? "</ol>" : "</ul>" }
                    html += "<ul style=\"list-style:none;padding-left:0;\">"; inList = true; listType = "ul"
                }
                let isChecked = trimmed.hasPrefix("- [x]") || trimmed.hasPrefix("- [X]")
                let label = String(trimmed.dropFirst(6))
                html += "<li class=\"task-item\(isChecked ? " checked" : "")\">"
                html += "<input type=\"checkbox\"\(isChecked ? " checked" : "") onchange=\"handleToggle(\(idx), this.checked)\">"
                html += "<span>" + inlineMarkdown(label) + "</span></li>"
            } else if trimmed.hasPrefix("- ") || trimmed.hasPrefix("* ") {
                if !inList || listType != "ul" {
                    if inList { html += listType == "ol" ? "</ol>" : "</ul>" }
                    html += "<ul>"; inList = true; listType = "ul"
                }
                html += "<li>" + inlineMarkdown(String(trimmed.dropFirst(2))) + "</li>"
            } else if let _ = trimmed.range(of: #"^\d+\.\s"#, options: .regularExpression) {
                if !inList || listType != "ol" {
                    if inList { html += listType == "ol" ? "</ol>" : "</ul>" }
                    html += "<ol>"; inList = true; listType = "ol"
                }
                let text = trimmed.replacingOccurrences(of: #"^\d+\.\s*"#, with: "", options: .regularExpression)
                html += "<li>" + inlineMarkdown(text) + "</li>"
            } else if trimmed.hasPrefix("> ") {
                if inList { html += listType == "ol" ? "</ol>" : "</ul>"; inList = false }
                html += "<blockquote>" + inlineMarkdown(String(trimmed.dropFirst(2))) + "</blockquote>"
            } else if trimmed.hasPrefix("![") {
                if inList { html += listType == "ol" ? "</ol>" : "</ul>"; inList = false }
                if let altEnd = trimmed.range(of: "]("),
                   let urlEnd = trimmed.range(of: ")", range: altEnd.upperBound..<trimmed.endIndex) {
                    let alt = String(trimmed[trimmed.index(trimmed.startIndex, offsetBy: 2)..<altEnd.lowerBound])
                    let src = String(trimmed[altEnd.upperBound..<urlEnd.lowerBound])
                    html += "<img src=\"" + escapeHTML(src) + "\" alt=\"" + escapeHTML(alt) + "\">"
                }
            } else {
                if inList { html += listType == "ol" ? "</ol>" : "</ul>"; inList = false }
                html += "<p>" + inlineMarkdown(trimmed) + "</p>"
            }
        }

        if inList { html += listType == "ol" ? "</ol>" : "</ul>" }
        if inCodeBlock { html += "<pre><code>" + escapeHTML(codeBlockContent) + "</code></pre>" }
        if inTable { html += renderTable(tableRows) }

        return html
    }

    func inlineMarkdown(_ text: String) -> String {
        var s = escapeHTML(text)

        let patterns: [(String, String, String)] = [
            (#"\*\*\*(.+?)\*\*\*"#, "<strong><em>", "</em></strong>"),
            (#"\*\*(.+?)\*\*"#, "<strong>", "</strong>"),
            (#"\*(.+?)\*"#, "<em>", "</em>"),
            (#"__(.+?)__"#, "<strong>", "</strong>"),
            (#"_(.+?)_"#, "<em>", "</em>"),
            (#"~~(.+?)~~"#, "<del>", "</del>"),
            (#"`(.+?)`"#, "<code>", "</code>"),
        ]

        for (pattern, open, close) in patterns {
            if let regex = try? NSRegularExpression(pattern: pattern) {
                let range = NSRange(s.startIndex..., in: s)
                s = regex.stringByReplacingMatches(in: s, range: range, withTemplate: open + "$1" + close)
            }
        }

        if let linkRegex = try? NSRegularExpression(pattern: #"\[(.+?)\]\((.+?)\)"#) {
            let range = NSRange(s.startIndex..., in: s)
            s = linkRegex.stringByReplacingMatches(in: s, range: range, withTemplate: "<a href=\"$2\" target=\"_blank\">$1</a>")
        }

        return s
    }

    func renderTable(_ rows: [[String]]) -> String {
        guard !rows.isEmpty else { return "" }
        var html = "<table>"
        for (i, row) in rows.enumerated() {
            html += "<tr>"
            let tag = i == 0 ? "th" : "td"
            for cell in row {
                html += "<\(tag)>" + inlineMarkdown(cell) + "</\(tag)>"
            }
            html += "</tr>"
        }
        html += "</table>"
        return html
    }

    func escapeHTML(_ s: String) -> String {
        s.replacingOccurrences(of: "&", with: "&amp;")
         .replacingOccurrences(of: "<", with: "&lt;")
         .replacingOccurrences(of: ">", with: "&gt;")
         .replacingOccurrences(of: "\"", with: "&quot;")
    }
}

class OpacityHandler: NSObject, WKScriptMessageHandler {
    weak var window: NSWindow?
    init(window: NSWindow) { self.window = window }

    func userContentController(_ controller: WKUserContentController, didReceive message: WKScriptMessage) {
        if let val = message.body as? String, let num = Double(val) {
            DispatchQueue.main.async { self.window?.alphaValue = CGFloat(num / 100.0) }
        }
    }
}

class ToggleHandler: NSObject, WKScriptMessageHandler {
    weak var controller: OverlayController?
    init(controller: OverlayController) { self.controller = controller }

    func userContentController(_ uc: WKUserContentController, didReceive message: WKScriptMessage) {
        guard let body = message.body as? String,
              let data = body.data(using: .utf8),
              let json = try? JSONSerialization.jsonObject(with: data) as? [String: Any],
              let line = json["line"] as? Int,
              let checked = json["checked"] as? Bool
        else { return }
        DispatchQueue.main.async { self.controller?.toggleCheckbox(lineIndex: line, checked: checked) }
    }
}

class EditHandler: NSObject, WKScriptMessageHandler {
    weak var controller: OverlayController?
    init(controller: OverlayController) { self.controller = controller }

    func userContentController(_ uc: WKUserContentController, didReceive message: WKScriptMessage) {
        guard let body = message.body as? String else { return }
        if body == "get" {
            let escaped = (controller?.fileContent ?? "")
                .replacingOccurrences(of: "\\", with: "\\\\")
                .replacingOccurrences(of: "`", with: "\\`")
                .replacingOccurrences(of: "$", with: "\\$")
            DispatchQueue.main.async {
                self.controller?.webView.evaluateJavaScript("showEditMode(`\(escaped)`)")
            }
        } else if body.hasPrefix("save:") {
            let newContent = String(body.dropFirst(5))
            DispatchQueue.main.async { self.controller?.saveContent(newContent) }
        }
    }
}

@main
struct PinnedOverlayApp {
    static func main() {
        let args = CommandLine.arguments
        guard args.count >= 2 else {
            print("Usage: pinned-overlay <file> [--x N] [--y N] [--width N] [--height N] [--opacity F]")
            exit(1)
        }

        let file = args[1]
        var x = 800, y = 200, w = 400, h = 500
        var opacity = 0.85

        var i = 2
        while i < args.count {
            switch args[i] {
            case "--x": i += 1; x = Int(args[i]) ?? x
            case "--y": i += 1; y = Int(args[i]) ?? y
            case "--width": i += 1; w = Int(args[i]) ?? w
            case "--height": i += 1; h = Int(args[i]) ?? h
            case "--opacity": i += 1; opacity = Double(args[i]) ?? opacity
            default: break
            }
            i += 1
        }

        let app = NSApplication.shared
        app.setActivationPolicy(.accessory)

        let _ = OverlayController(filePath: file, x: x, y: y, w: w, h: h, opacity: opacity)

        app.run()
    }
}
