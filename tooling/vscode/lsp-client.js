const cp = require('child_process');
const path = require('path');

function encodeRpcMessage(payload) {
  const json = JSON.stringify(payload);
  const bytes = Buffer.from(json, 'utf8');
  return Buffer.concat([
    Buffer.from(`Content-Length: ${bytes.length}\r\n\r\n`, 'ascii'),
    bytes,
  ]);
}

function createRpcParser(onMessage) {
  let buffer = Buffer.alloc(0);
  return function push(chunk) {
    buffer = Buffer.concat([buffer, chunk]);
    while (true) {
      const headerEnd = buffer.indexOf('\r\n\r\n');
      if (headerEnd === -1) break;
      const header = buffer.slice(0, headerEnd).toString('ascii');
      const match = /Content-Length:\s*(\d+)/i.exec(header);
      if (!match) {
        buffer = buffer.slice(headerEnd + 4);
        continue;
      }
      const length = parseInt(match[1], 10);
      const total = headerEnd + 4 + length;
      if (buffer.length < total) break;
      const body = buffer.slice(headerEnd + 4, total).toString('utf8');
      buffer = buffer.slice(total);
      try {
        onMessage(JSON.parse(body));
      } catch (_) {
        // Ignore malformed server frames.
      }
    }
  };
}

function resolveExecutable(configuredPath, workspaceFolders) {
  if (configuredPath && configuredPath.trim()) return configuredPath.trim();
  if (process.env.VEYRA_EXECUTABLE) return process.env.VEYRA_EXECUTABLE;
  const fs = require('fs');
  if (Array.isArray(workspaceFolders)) {
    for (const folder of workspaceFolders) {
      const root = typeof folder === 'string' ? folder : folder && folder.uri && folder.uri.fsPath;
      if (!root) continue;
      const candidate = path.join(root, 'bin', process.platform === 'win32' ? 'veyra-native.exe' : 'veyra');
      if (fs.existsSync(candidate)) return candidate;
    }
  }
  if (process.platform === 'win32' && process.env.LOCALAPPDATA) {
    const installed = path.join(process.env.LOCALAPPDATA, 'Veyra', 'veyra.bat');
    if (fs.existsSync(installed)) return installed;
  }
  return 'veyra';
}

class VeyraLspClient {
  constructor(commandResolver, onNotification) {
    this.commandResolver = commandResolver;
    this.onNotification = onNotification;
    this.proc = null;
    this.nextId = 1;
    this.pending = new Map();
    this.initPromise = null;
    this.openedUris = new Map();
  }

  async ensureStarted() {
    if (this.proc && !this.proc.killed) {
      return this.initPromise;
    }
    const exe = this.commandResolver();
    const proc = cp.spawn(exe, ['lsp'], {
      stdio: ['pipe', 'pipe', 'pipe'],
      windowsHide: true,
    });
    this.proc = proc;
    const feed = createRpcParser((msg) => this._handleMessage(msg));
    proc.stdout.on('data', feed);
    proc.on('exit', () => {
      for (const entry of this.pending.values()) {
        clearTimeout(entry.timer);
        entry.reject(new Error('Veyra LSP process exited'));
      }
      this.pending.clear();
      this.proc = null;
      this.initPromise = null;
    });
    proc.on('error', () => {
      for (const entry of this.pending.values()) {
        clearTimeout(entry.timer);
        entry.reject(new Error('Failed to start Veyra LSP'));
      }
      this.pending.clear();
      this.proc = null;
      this.initPromise = null;
    });

    this.initPromise = this._rawRequest('initialize', {
      processId: process.pid,
      rootUri: null,
      capabilities: {
        textDocument: {
          publishDiagnostics: {},
          synchronization: { dynamicRegistration: false },
          completion: { completionItem: { snippetSupport: false } },
          hover: {},
          definition: {},
          references: {},
          rename: {},
          documentSymbol: { hierarchicalDocumentSymbolSupport: true },
          formatting: {},
        },
      },
    }).then((res) => {
      this.notify('initialized', {});
      return res;
    });
    return this.initPromise;
  }

  _handleMessage(msg) {
    if (typeof msg.id !== 'undefined' && this.pending.has(msg.id)) {
      const entry = this.pending.get(msg.id);
      this.pending.delete(msg.id);
      clearTimeout(entry.timer);
      if (msg.error) {
        entry.reject(new Error(msg.error.message || 'LSP error'));
      } else {
        entry.resolve(msg.result);
      }
      return;
    }
    if (msg.method && this.onNotification) {
      this.onNotification(msg.method, msg.params);
    }
  }

  _rawRequest(method, params, timeoutMs = 4000) {
    if (!this.proc || !this.proc.stdin.writable) {
      return Promise.reject(new Error('LSP transport unavailable'));
    }
    const id = this.nextId++;
    return new Promise((resolve, reject) => {
      const timer = setTimeout(() => {
        this.pending.delete(id);
        reject(new Error(`LSP request timed out: ${method}`));
      }, timeoutMs);
      this.pending.set(id, { resolve, reject, timer });
      this.proc.stdin.write(
        encodeRpcMessage({ jsonrpc: '2.0', id, method, params })
      );
    });
  }

  async request(method, params, timeoutMs = 4000) {
    await this.ensureStarted();
    return this._rawRequest(method, params, timeoutMs);
  }

  notify(method, params) {
    if (!this.proc || !this.proc.stdin.writable) return;
    this.proc.stdin.write(
      encodeRpcMessage({ jsonrpc: '2.0', method, params })
    );
  }

  async didOpen(uri, text, version = 1) {
    await this.ensureStarted();
    this.openedUris.set(uri, version);
    this.notify('textDocument/didOpen', {
      textDocument: { uri, languageId: 'veyra', version, text },
    });
  }

  async didChange(uri, text, version) {
    await this.ensureStarted();
    this.openedUris.set(uri, version);
    this.notify('textDocument/didChange', {
      textDocument: { uri, version },
      contentChanges: [{ text }],
    });
  }

  didClose(uri) {
    if (!this.openedUris.has(uri)) return;
    this.openedUris.delete(uri);
    this.notify('textDocument/didClose', { textDocument: { uri } });
  }

  async stop() {
    if (!this.proc) return;
    const p = this.proc;
    try {
      await this._rawRequest('shutdown', null, 1000);
      this.notify('exit', null);
    } catch (_) {
      // Ignore shutdown errors and proceed to tear down handles.
    }
    this.proc = null;
    this.initPromise = null;
    try {
      p.stdin.end();
    } catch (_) {}
    p.stdout.destroy();
    p.stderr.destroy();
    if (!p.killed) p.kill();
  }
}

module.exports = {
  encodeRpcMessage,
  createRpcParser,
  resolveExecutable,
  VeyraLspClient,
};
