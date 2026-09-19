const vscode = require('vscode');
const cp = require('child_process');

const keywords = [
  'let','const','fn','class','if','else','while','for','in','return','break','continue',
  'try','catch','throw','use','true','false','null'
];
const builtins = [
  'print','len','str','int','float','bool','type','range','assert','push','pop','keys','values',
  'json','json_parse','regex','py_import','py_call','py_get','py_set','py_dir','parallel_map',
  'spawn','await_task','task_done','http_get','db_open','shell','read','write','sqrt','sum','sorted'
];

function executable() {
  return process.env.LOCALAPPDATA + '\\Veyra\\veyra.bat';
}

function checkDocument(doc, diagnostics) {
  if (doc.languageId !== 'veyra' || doc.isUntitled) return;
  cp.execFile(executable(), ['check', doc.fileName], {windowsHide:true}, (err, stdout, stderr) => {
    const items = [];
    if (err) {
      const text = (stderr || stdout || err.message).trim();
      const range = new vscode.Range(0,0,Math.max(0,doc.lineCount-1),Math.max(1,doc.lineAt(Math.max(0,doc.lineCount-1)).text.length));
      items.push(new vscode.Diagnostic(range, text, vscode.DiagnosticSeverity.Error));
    }
    diagnostics.set(doc.uri, items);
  });
}

function activate(context) {
  const diagnostics = vscode.languages.createDiagnosticCollection('veyra');
  context.subscriptions.push(diagnostics);

  vscode.workspace.textDocuments.forEach(d => checkDocument(d, diagnostics));
  context.subscriptions.push(vscode.workspace.onDidSaveTextDocument(d => checkDocument(d, diagnostics)));
  context.subscriptions.push(vscode.workspace.onDidOpenTextDocument(d => checkDocument(d, diagnostics)));

  context.subscriptions.push(vscode.commands.registerCommand('veyra.check', () => {
    const ed = vscode.window.activeTextEditor;
    if (!ed || ed.document.languageId !== 'veyra') return;
    ed.document.save().then(() => checkDocument(ed.document, diagnostics));
  }));

  context.subscriptions.push(vscode.languages.registerCompletionItemProvider('veyra', {
    provideCompletionItems() {
      return [
        ...keywords.map(k => new vscode.CompletionItem(k, vscode.CompletionItemKind.Keyword)),
        ...builtins.map(k => new vscode.CompletionItem(k, vscode.CompletionItemKind.Function))
      ];
    }
  }));

  context.subscriptions.push(vscode.languages.registerHoverProvider('veyra', {
    provideHover(doc, pos) {
      const range = doc.getWordRangeAtPosition(pos);
      if (!range) return;
      const w = doc.getText(range);
      if (w === 'py_import') return new vscode.Hover('**py_import(name)** — import a project-installed Python module into Veyra.');
      if (w === 'spawn') return new vscode.Hover('**spawn(fn, ...args)** — run a callable as a background task.');
      if (w === 'await_task') return new vscode.Hover('**await_task(task)** — wait for and return a spawned task result.');
      if (w === 'class') return new vscode.Hover('**class** — declare a Veyra class; use `init(self, ...)` as its constructor.');
      if (keywords.includes(w)) return new vscode.Hover('Veyra keyword: **' + w + '**');
    }
  }));

  context.subscriptions.push(vscode.languages.registerDocumentFormattingEditProvider('veyra', {
    provideDocumentFormattingEdits(doc) {
      return new Promise(resolve => {
        cp.execFile(executable(), ['fmt', doc.fileName], {windowsHide:true}, err => {
          if (err) return resolve([]);
          const fs = require('fs');
          const text = fs.readFileSync(doc.fileName, 'utf8');
          const full = new vscode.Range(doc.positionAt(0), doc.positionAt(doc.getText().length));
          resolve([vscode.TextEdit.replace(full, text)]);
        });
      });
    }
  }));
}

function deactivate() {}
module.exports = { activate, deactivate };
