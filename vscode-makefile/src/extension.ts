import * as path from 'path';
import * as fs from 'fs';
import { workspace, ExtensionContext } from 'vscode';
import {
  LanguageClient,
  LanguageClientOptions,
  ServerOptions,
  TransportKind
} from 'vscode-languageclient/node';

let client: LanguageClient;

function getBundledServerPath(context: ExtensionContext): string | undefined {
  const ext = process.platform === 'win32' ? '.exe' : '';
  const binaryPath = path.join(context.extensionPath, 'server', `makefile-lsp${ext}`);
  if (fs.existsSync(binaryPath)) {
    return binaryPath;
  }
  return undefined;
}

export function activate(context: ExtensionContext) {
  const config = workspace.getConfiguration('makefile');
  const isEnable = config.get<boolean>('enable', true);

  if (!isEnable) {
    return;
  }

  const configuredPath = config.get<string>('serverPath', '');
  const serverPath = configuredPath || getBundledServerPath(context) || 'makefile-lsp';

  const serverOptions: ServerOptions = {
    command: serverPath,
    args: [],
    transport: TransportKind.stdio
  };

  const clientOptions: LanguageClientOptions = {
    documentSelector: [
      { scheme: 'file', language: 'makefile' },
    ],
    synchronize: {
      fileEvents: workspace.createFileSystemWatcher('**/Makefile')
    }
  };

  client = new LanguageClient(
    'makefile',
    'Makefile Language Server',
    serverOptions,
    clientOptions
  );

  client.start();
}

export function deactivate(): Thenable<void> | undefined {
  if (!client) {
    return undefined;
  }
  return client.stop();
}
