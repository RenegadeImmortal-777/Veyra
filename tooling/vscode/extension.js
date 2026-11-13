const vscode = require('vscode');
const cp = require('child_process');
const { VeyraLspClient, resolveExecutable } = require('./lsp-client');

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
  'spawn','await_task','await_timeout','gather','task_done','task_status','channel','send','recv','try_recv','sleep_ms','cpu_count',
  'platform_os','platform_arch','platform_family','path_separator',
  'path_join','path_name','path_parent','path_extension','starts_with','ends_with','replace_text',
  'accuracy_score','classification_metrics','train_test_split',
  'tensor','tensor_values','tensor_shape','tensor_grad','tensor_add','tensor_sub','tensor_mul','tensor_div','tensor_neg','tensor_pow','tensor_matmul','tensor_softmax','tensor_detach','tensor_numel','tensor_relu','tensor_sigmoid','tensor_tanh','tensor_exp','tensor_log','tensor_reshape','tensor_transpose','tensor_scale','tensor_cross_entropy','tensor_layer_norm','tensor_conv1d','tensor_conv2d','tensor_attention','tensor_multihead_attention','tensor_checkpoint_save','tensor_checkpoint_load','tensor_save','tensor_load','tensor_sum','tensor_mean','tensor_backward','tensor_step','tensor_adam_step','tensor_reset_optimizer','tensor_zero_grad',
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

let lspClient = null;

function executable() {
  const configured = vscode.workspace.getConfiguration('veyra').get('executablePath');
  return resolveExecutable(configured, vscode.workspace.workspaceFolders);
}

function toVscodeRange(r) {
  return new vscode.Range(r.start.line, r.start.character, r.end.line, r.end.character);
}

function toVscodeLocation(loc) {
  return new vscode.Location(vscode.Uri.parse(loc.uri), toVscodeRange(loc.range));
}

function toDocumentSymbol(sym) {
  const kindMap = {
    5: vscode.SymbolKind.Class,
    6: vscode.SymbolKind.Method,
    12: vscode.SymbolKind.Function,
    13: vscode.SymbolKind.Variable,
  };
  const ds = new vscode.DocumentSymbol(
    sym.name,
    '',
    kindMap[sym.kind] || vscode.SymbolKind.Variable,
    toVscodeRange(sym.range),
    toVscodeRange(sym.selectionRange || sym.range)
  );
  if (Array.isArray(sym.children)) {
    ds.children = sym.children.map(toDocumentSymbol);
  }
  return ds;
}

function runInTerminal(args, name) {
  const terminal = vscode.window.createTerminal(name || 'Veyra');
  terminal.show();
  const exe = executable();
  const quotedExe = exe.includes(' ') ? '"' + exe + '"' : exe;
  const rendered = args.map(x => '"' + String(x).replace(/"/g, '\\"') + '"').join(' ');
  terminal.sendText(quotedExe + ' ' + rendered);
}

function fallbackDefinitionLocation(doc, word) {
  const patterns = [
    new RegExp('\\b(?:async\\s+)?(?:fn|def)\\s+' + word + '\\b'),
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

function fallbackReferenceLocations(doc, word) {
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

function fallbackDocumentSymbols(doc) {
  const out = [];
  for (let i = 0; i < doc.lineCount; i++) {
    const text = doc.lineAt(i).text;
    let m = text.match(/^\s*(?:async\s+)?(?:fn|def)\s+([A-Za-z_][A-Za-z0-9_]*)/);
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

function checkDocumentFallback(doc, diagnostics) {
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

  lspClient = new VeyraLspClient(executable, (method, params) => {
    if (method === 'textDocument/publishDiagnostics' && params && params.uri) {
      const uri = vscode.Uri.parse(params.uri);
      const list = (params.diagnostics || []).map((d) => {
        const sev = d.severity === 1
          ? vscode.DiagnosticSeverity.Error
          : d.severity === 2
            ? vscode.DiagnosticSeverity.Warning
            : vscode.DiagnosticSeverity.Information;
        return new vscode.Diagnostic(toVscodeRange(d.range), d.message, sev);
      });
      diagnostics.set(uri, list);
    }
  });

  const syncOpen = (doc) => {
    if (doc.languageId !== 'veyra') return;
    lspClient.didOpen(doc.uri.toString(), doc.getText(), doc.version)
      .catch(() => checkDocumentFallback(doc, diagnostics));
  };

  vscode.workspace.textDocuments.forEach(syncOpen);
  context.subscriptions.push(vscode.workspace.onDidOpenTextDocument(syncOpen));
  context.subscriptions.push(vscode.workspace.onDidChangeTextDocument((e) => {
    if (e.document.languageId !== 'veyra') return;
    lspClient.didChange(e.document.uri.toString(), e.document.getText(), e.document.version).catch(() => {});
  }));
  context.subscriptions.push(vscode.workspace.onDidSaveTextDocument((doc) => {
    if (doc.languageId !== 'veyra') return;
    lspClient.didChange(doc.uri.toString(), doc.getText(), doc.version)
      .catch(() => checkDocumentFallback(doc, diagnostics));
  }));
  context.subscriptions.push(vscode.workspace.onDidCloseTextDocument((doc) => {
    if (doc.languageId === 'veyra') {
      lspClient.didClose(doc.uri.toString());
      diagnostics.delete(doc.uri);
    }
  }));

  context.subscriptions.push(vscode.debug.registerDebugAdapterDescriptorFactory('veyra', {
    createDebugAdapterDescriptor() {
      return new vscode.DebugAdapterExecutable(executable(), ['dap']);
    }
  }));
  context.subscriptions.push(vscode.debug.registerDebugConfigurationProvider('veyra', {
    resolveDebugConfiguration(folder, config) {
      if (!config.type && !config.request && !config.name) {
        const ed = vscode.window.activeTextEditor;
        if (ed && ed.document.languageId === 'veyra') {
          config.type = 'veyra';
          config.name = 'Debug Current Veyra File';
          config.request = 'launch';
          config.program = ed.document.fileName;
          config.stopOnEntry = false;
        }
      }
      return config;
    }
  }));

  context.subscriptions.push(vscode.commands.registerCommand('veyra.check', () => {
    const ed = vscode.window.activeTextEditor;
    if (!ed || ed.document.languageId !== 'veyra') return;
    ed.document.save().then(() => {
      lspClient.didChange(ed.document.uri.toString(), ed.document.getText(), ed.document.version)
        .catch(() => checkDocumentFallback(ed.document, diagnostics));
    });
  }));
  context.subscriptions.push(vscode.commands.registerCommand('veyra.run', () => {
    const ed = vscode.window.activeTextEditor;
    if (ed && ed.document.languageId === 'veyra') runInTerminal(['run', ed.document.fileName], 'Veyra Run');
  }));
  context.subscriptions.push(vscode.commands.registerCommand('veyra.debug', () => {
    const ed = vscode.window.activeTextEditor;
    if (!ed || ed.document.languageId !== 'veyra') return;
    vscode.debug.startDebugging(undefined, {
      type: 'veyra',
      request: 'launch',
      name: 'Debug Current Veyra File',
      program: ed.document.fileName,
      stopOnEntry: true,
    });
  }));
  context.subscriptions.push(vscode.commands.registerCommand('veyra.profile', () => {
    const ed = vscode.window.activeTextEditor;
    if (ed && ed.document.languageId === 'veyra') runInTerminal(['profile', ed.document.fileName], 'Veyra Profile');
  }));
  context.subscriptions.push(vscode.commands.registerCommand('veyra.test', () => {
    runInTerminal(['test'], 'Veyra Tests');
  }));

  context.subscriptions.push(vscode.languages.registerCompletionItemProvider('veyra', {
    async provideCompletionItems(doc, pos) {
      try {
        const items = await lspClient.request('textDocument/completion', {
          textDocument: { uri: doc.uri.toString() },
          position: { line: pos.line, character: pos.character },
        });
        if (Array.isArray(items) && items.length > 0) {
          return items.map((item) => {
            const kind = item.kind === 14 ? vscode.CompletionItemKind.Keyword : vscode.CompletionItemKind.Function;
            const ci = new vscode.CompletionItem(item.label, kind);
            if (item.detail) ci.detail = item.detail;
            return ci;
          });
        }
      } catch (_) {}
      return [
        ...keywords.map(k => new vscode.CompletionItem(k, vscode.CompletionItemKind.Keyword)),
        ...builtins.map(k => new vscode.CompletionItem(k, vscode.CompletionItemKind.Function))
      ];
    }
  }));

  context.subscriptions.push(vscode.languages.registerDefinitionProvider('veyra', {
    async provideDefinition(doc, pos) {
      try {
        const res = await lspClient.request('textDocument/definition', {
          textDocument: { uri: doc.uri.toString() },
          position: { line: pos.line, character: pos.character },
        });
        if (res && res.uri && res.range) return toVscodeLocation(res);
      } catch (_) {}
      const range = doc.getWordRangeAtPosition(pos);
      if (!range) return;
      return fallbackDefinitionLocation(doc, doc.getText(range));
    }
  }));

  context.subscriptions.push(vscode.languages.registerReferenceProvider('veyra', {
    async provideReferences(doc, pos) {
      try {
        const res = await lspClient.request('textDocument/references', {
          textDocument: { uri: doc.uri.toString() },
          position: { line: pos.line, character: pos.character },
        });
        if (Array.isArray(res)) return res.map(toVscodeLocation);
      } catch (_) {}
      const range = doc.getWordRangeAtPosition(pos);
      if (!range) return [];
      return fallbackReferenceLocations(doc, doc.getText(range));
    }
  }));

  context.subscriptions.push(vscode.languages.registerRenameProvider('veyra', {
    async provideRenameEdits(doc, pos, newName) {
      if (!/^[A-Za-z_][A-Za-z0-9_]*$/.test(newName)) return;
      try {
        const res = await lspClient.request('textDocument/rename', {
          textDocument: { uri: doc.uri.toString() },
          position: { line: pos.line, character: pos.character },
          newName,
        });
        if (res && res.changes) {
          const edit = new vscode.WorkspaceEdit();
          for (const [uriStr, edits] of Object.entries(res.changes)) {
            const uri = vscode.Uri.parse(uriStr);
            for (const e of edits) {
              edit.replace(uri, toVscodeRange(e.range), e.newText);
            }
          }
          return edit;
        }
      } catch (_) {}
      const range = doc.getWordRangeAtPosition(pos);
      if (!range) return;
      const word = doc.getText(range);
      const edit = new vscode.WorkspaceEdit();
      for (const loc of fallbackReferenceLocations(doc, word)) {
        edit.replace(doc.uri, loc.range, newName);
      }
      return edit;
    }
  }));

  context.subscriptions.push(vscode.languages.registerDocumentSymbolProvider('veyra', {
    async provideDocumentSymbols(doc) {
      try {
        const res = await lspClient.request('textDocument/documentSymbol', {
          textDocument: { uri: doc.uri.toString() },
        });
        if (Array.isArray(res)) return res.map(toDocumentSymbol);
      } catch (_) {}
      return fallbackDocumentSymbols(doc);
    }
  }));

  context.subscriptions.push(vscode.languages.registerHoverProvider('veyra', {
    async provideHover(doc, pos) {
      try {
        const res = await lspClient.request('textDocument/hover', {
          textDocument: { uri: doc.uri.toString() },
          position: { line: pos.line, character: pos.character },
        });
        if (res && res.contents && res.contents.value) {
          return new vscode.Hover(res.contents.value);
        }
      } catch (_) {}
      const range = doc.getWordRangeAtPosition(pos);
      if (!range) return;
      const w = doc.getText(range);
      if (keywords.includes(w)) return new vscode.Hover('Veyra keyword: **' + w + '**');
    }
  }));

  context.subscriptions.push(vscode.languages.registerDocumentFormattingEditProvider('veyra', {
    async provideDocumentFormattingEdits(doc) {
      try {
        const edits = await lspClient.request('textDocument/formatting', {
          textDocument: { uri: doc.uri.toString() },
        });
        if (Array.isArray(edits)) {
          return edits.map((e) => vscode.TextEdit.replace(toVscodeRange(e.range), e.newText));
        }
      } catch (_) {}
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

function deactivate() {
  if (lspClient) {
    return lspClient.stop();
  }
}

module.exports = { activate, deactivate };
