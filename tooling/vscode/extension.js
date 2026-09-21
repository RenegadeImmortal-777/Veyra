const vscode = require('vscode');
const cp = require('child_process');

const keywords = [
  'let','const','fn','async','class','if','else','while','for','in','return','use',
  'try','catch','finally','throw','with','as','break','continue','await','yield','match','case',
  'and','or','not','true','false','null'
];
const builtins = [
  'print','len','str','int','float','bool','assert','sum','min','max','all','any',
  'range','keys','values','get','tuple','set','contains','index_of','append','concat','slice','reverse','sort','unique','enumerate','zip','map_call','filter_call','reduce_call',
  'type_of','is_instance','has_field',
  'json_parse','json_stringify','regex_findall',
  'sqrt','abs','floor','ceil','lower','upper','trim','split','join','contains',
  'read_file','write_file','path_exists','mkdir','remove_file','list_dir','cwd','env_get',
  'http_get','http_post','http_request','shell','clock','random','randint','sha256','hmac_sha256','pbkdf2_sha256','constant_time_equal','sqlite_exec','sqlite_query','sqlite_exec_params','sqlite_query_params','import_package',
  'spawn','await_task','gather','task_done','channel','send','recv','try_recv','sleep_ms','cpu_count',
  'platform_os','platform_arch','platform_family','path_separator',
  'path_join','path_name','path_parent','path_extension','starts_with','ends_with','replace_text',
  'accuracy_score','classification_metrics','train_test_split',
  'tensor','tensor_values','tensor_shape','tensor_grad','tensor_add','tensor_sub','tensor_mul','tensor_matmul','tensor_relu','tensor_sum','tensor_mean','tensor_backward','tensor_step','tensor_zero_grad',
  'next','collect','generator_done',
  'toml_parse','toml_stringify','base64_encode','base64_decode','uuid_v4','now_utc','now_local','datetime_format',
  'ffi_available','ffi_i32','ffi_i64','ffi_f64',
  'vec_sum','vec_mean','vec_variance','vec_std','vec_median','vec_percentile','covariance','correlation','vec_dot','vec_add','vec_scale',
  'transpose','matmul','matrix_det','matrix_inverse','linear_solve','linreg_fit','linreg_predict','knn_predict','kmeans',
  'normalize','dense','relu','softmax','sigmoid','mse','logreg_fit','logreg_predict','csv_read','csv_write',
  'table_column','table_filter_eq','table_describe','table_head','table_tail','table_drop_nulls','table_fill_null','table_group_count','table_group_sum','table_group_mean','table_sort','table_select','table_inner_join','table_left_join',
  'plot_svg','scatter_svg','bar_svg','histogram','regex_match','regex_replace','regex_split',
  'set_union','set_intersection','set_difference',
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

function definitionLocation(doc, word) {
  const patterns = [
    new RegExp('\\bfn\\s+' + word + '\\b'),
    new RegExp('\\bclass\\s+' + word + '\\b'),
    new RegExp('\\b(?:let|const)\\s+' + word + '\\b')
  ];
  for (let i = 0; i < doc.lineCount; i++) {
    const text = doc.lineAt(i).text;
    if (patterns.some(r => r.test(text))) {
      const c = text.indexOf(word);
      if (c >= 0) return new vscode.Location(doc.uri, new vscode.Range(i,c,i,c+word.length));
    }
  }
}

function referenceLocations(doc, word) {
  const out = [];
  const re = new RegExp('\\b' + word + '\\b', 'g');
  for (let i = 0; i < doc.lineCount; i++) {
    const text = doc.lineAt(i).text;
    let m;
    while ((m = re.exec(text)) !== null) {
      out.push(new vscode.Location(doc.uri, new vscode.Range(i,m.index,i,m.index+word.length)));
    }
  }
  return out;
}

function documentSymbols(doc) {
  const out = [];
  for (let i = 0; i < doc.lineCount; i++) {
    const text = doc.lineAt(i).text;
    let m = text.match(/^\s*(?:async\s+)?fn\s+([A-Za-z_][A-Za-z0-9_]*)/);
    let kind = vscode.SymbolKind.Function;
    if (!m) { m = text.match(/^\s*class\s+([A-Za-z_][A-Za-z0-9_]*)/); kind = vscode.SymbolKind.Class; }
    if (!m) { m = text.match(/^\s*(?:let|const)\s+([A-Za-z_][A-Za-z0-9_]*)/); kind = vscode.SymbolKind.Variable; }
    if (m) {
      const c = text.indexOf(m[1]);
      out.push(new vscode.DocumentSymbol(m[1], '', kind, new vscode.Range(i,0,i,text.length), new vscode.Range(i,c,i,c+m[1].length)));
    }
  }
  return out;
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

  context.subscriptions.push(vscode.languages.registerDefinitionProvider('veyra', {
    provideDefinition(doc, pos) {
      const range = doc.getWordRangeAtPosition(pos);
      if (!range) return;
      return definitionLocation(doc, doc.getText(range));
    }
  }));

  context.subscriptions.push(vscode.languages.registerReferenceProvider('veyra', {
    provideReferences(doc, pos) {
      const range = doc.getWordRangeAtPosition(pos);
      if (!range) return [];
      return referenceLocations(doc, doc.getText(range));
    }
  }));

  context.subscriptions.push(vscode.languages.registerRenameProvider('veyra', {
    provideRenameEdits(doc, pos, newName) {
      const range = doc.getWordRangeAtPosition(pos);
      if (!range || !/^[A-Za-z_][A-Za-z0-9_]*$/.test(newName)) return;
      const word = doc.getText(range);
      const edit = new vscode.WorkspaceEdit();
      for (const loc of referenceLocations(doc, word)) {
        edit.replace(doc.uri, loc.range, newName);
      }
      return edit;
    }
  }));

  context.subscriptions.push(vscode.languages.registerDocumentSymbolProvider('veyra', {
    provideDocumentSymbols(doc) {
      return documentSymbols(doc);
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
