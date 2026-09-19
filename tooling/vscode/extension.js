const vscode = require('vscode');
const cp = require('child_process');

const keywords = [
  'let','const','fn','class','if','else','while','for','in','return','use',
  'try','catch','throw','break','continue','await',
  'and','or','not','true','false','null'
];
const builtins = [
  'print','len','str','int','float','bool','assert','sum','min','max','all','any',
  'range','keys','values','get','tuple','set','contains','index_of','append','concat','slice','reverse','sort','unique','enumerate','zip','map_call','filter_call','reduce_call',
  'type_of','is_instance','has_field',
  'json_parse','json_stringify','regex_findall',
  'sqrt','abs','floor','ceil','lower','upper','trim','split','join','contains',
  'read_file','write_file','path_exists','mkdir','remove_file','list_dir','cwd','env_get',
  'http_get','http_post','shell','clock','random','randint','sha256','sqlite_exec','sqlite_query',
  'spawn','await_task','task_done','channel','send','recv','try_recv','sleep_ms','cpu_count',
  'ffi_available','ffi_i32','ffi_i64','ffi_f64',
  'vec_sum','vec_mean','vec_variance','vec_std','vec_median','vec_percentile','covariance','correlation','vec_dot','vec_add','vec_scale',
  'transpose','matmul','linreg_fit','linreg_predict','knn_predict','kmeans',
  'normalize','dense','relu','softmax','sigmoid','mse','logreg_fit','logreg_predict','csv_read','csv_write',
  'table_column','table_filter_eq','table_describe','table_group_count','table_sort','table_select','table_inner_join','plot_svg',
  'gui_message','gui_confirm','gui_input','gui_password','gui_form',
  'gui_open_file','gui_save_file','gui_folder','gui_color'
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
      if (w === 'linreg_fit') return new vscode.Hover('**linreg_fit(x, y)** — fit native simple linear regression and return a model map.');
      if (w === 'kmeans') return new vscode.Hover('**kmeans(data, k, iterations)** — native k-means clustering.');
      if (w === 'dense') return new vscode.Hover('**dense(input, weights, bias)** — native dense neural-layer inference.');
      if (w === 'csv_read') return new vscode.Hover('**csv_read(path)** — load a CSV file into Veyra row maps.');
      if (w === 'gui_form') return new vscode.Hover('**gui_form(title, fields)** — collect multiple values using native desktop dialogs.');
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
