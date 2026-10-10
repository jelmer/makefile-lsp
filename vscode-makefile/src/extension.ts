import * as path from 'path';
import * as fs from 'fs';
import {
  commands,
  workspace,
  window,
  ExtensionContext,
  ProcessExecution,
  Task,
  TaskDefinition,
  TaskScope,
  tasks,
  Uri
} from 'vscode';
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

const TASK_TYPE = 'makefile-lsp';

interface RunTargetDefinition extends TaskDefinition {
  target: string;
  makefile: string;
  directory?: string;
}

function makeTask(definition: RunTargetDefinition): Task {
  const program = workspace.getConfiguration('makefile').get<string>('makeProgram', 'make');
  // nmake takes /F rather than -f.
  const isNmake = path.basename(program).toLowerCase().replace(/\.exe$/, '') === 'nmake';
  const args = [isNmake ? '/F' : '-f', definition.makefile, definition.target];
  const cwd = definition.directory ?? path.dirname(definition.makefile);
  const folder = workspace.getWorkspaceFolder(Uri.file(definition.makefile));
  return new Task(
    definition,
    folder ?? TaskScope.Workspace,
    definition.target,
    'make',
    new ProcessExecution(program, args, { cwd })
  );
}

function registerRunTarget(context: ExtensionContext) {
  context.subscriptions.push(
    tasks.registerTaskProvider(TASK_TYPE, {
      provideTasks: () => [],
      resolveTask: (task) => makeTask(task.definition as RunTargetDefinition)
    }),
    commands.registerCommand(
      'makefile-lsp.runTarget',
      async (args?: { makefile?: string; directory?: string; target?: string }) => {
        if (!args?.makefile || !args.target) {
          window.showErrorMessage('makefile-lsp.runTarget needs a makefile and a target');
          return;
        }
        await tasks.executeTask(makeTask({
          type: TASK_TYPE,
          makefile: args.makefile,
          directory: args.directory,
          target: args.target
        }));
      }
    )
  );
}

export function activate(context: ExtensionContext) {
  const config = workspace.getConfiguration('makefile');
  const isEnable = config.get<boolean>('enable', true);

  if (!isEnable) {
    return;
  }

  const configuredPath = config.get<string>('serverPath', '');
  const serverPath = configuredPath || getBundledServerPath(context) || 'makefile-lsp';

  registerRunTarget(context);

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
    },
    initializationOptions: {
      codeLens: {
        runTarget: config.get<boolean>('codeLens.runTarget', true)
      }
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
