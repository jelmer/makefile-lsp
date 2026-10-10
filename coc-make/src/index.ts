import * as path from 'path';
import {
  commands,
  ExtensionContext,
  LanguageClient,
  LanguageClientOptions,
  ServerOptions,
  services,
  window,
  workspace
} from 'coc.nvim';

interface RunTargetArgs {
  makefile?: string;
  directory?: string;
  target?: string;
}

/**
 * Run a target in a terminal, for the "Run" code lenses.
 */
async function runTarget(args?: RunTargetArgs): Promise<void> {
  if (!args?.makefile || !args.target) {
    window.showErrorMessage('makefile-lsp.runTarget needs a makefile and a target');
    return;
  }
  const program = workspace.getConfiguration('make').get<string>('makeProgram', 'make');
  // nmake takes /F rather than -f.
  const isNmake = path.basename(program).toLowerCase().replace(/\.exe$/, '') === 'nmake';
  const terminal = await window.createTerminal({
    name: `make ${args.target}`,
    shellPath: program,
    shellArgs: [isNmake ? '/F' : '-f', args.makefile, args.target],
    cwd: args.directory ?? path.dirname(args.makefile)
  });
  await terminal.show(true);
}

/**
 * Set up highlight links for semantic token types.
 *
 * coc.nvim creates highlight groups named CocSemType<tokenType> for each
 * semantic token type reported by the server. By default only standard LSP
 * types get linked, so we link the custom makefile-lsp types to Vim groups.
 */
function setupSemanticHighlights(): void {
  const { nvim } = workspace;

  const links: Record<string, string> = {
    CocSemTypemakefileTarget: 'Function',
    CocSemTypemakefileVariable: 'Identifier',
    CocSemTypemakefilePrerequisite: 'Type',
    CocSemTypemakefileRecipe: 'String',
  };

  for (const [group, target] of Object.entries(links)) {
    nvim.command(`hi default link ${group} ${target}`, true);
  }
}

export async function activate(context: ExtensionContext): Promise<void> {
  const config = workspace.getConfiguration('make');
  const isEnable = config.get<boolean>('enable', true);

  if (!isEnable) {
    return;
  }

  setupSemanticHighlights();

  context.subscriptions.push(
    commands.registerCommand('makefile-lsp.runTarget', runTarget, null, true)
  );

  const serverPath = config.get<string>('serverPath', 'makefile-lsp');

  const serverOptions: ServerOptions = {
    command: serverPath,
    args: []
  };

  const clientOptions: LanguageClientOptions = {
    documentSelector: [
      { scheme: 'file', language: 'make' },
      { scheme: 'file', pattern: '**/Makefile' },
      { scheme: 'file', pattern: '**/makefile' },
      { scheme: 'file', pattern: '**/GNUmakefile' },
      { scheme: 'file', pattern: '**/*.mk' },
    ],
    synchronize: {
      fileEvents: workspace.createFileSystemWatcher('**/{Makefile,makefile,GNUmakefile,*.mk}')
    },
    initializationOptions: {
      codeLens: {
        runTarget: config.get<boolean>('codeLens.runTarget', true)
      }
    }
  };

  const client = new LanguageClient(
    'makefile',
    'Makefile Language Server',
    serverOptions,
    clientOptions
  );

  context.subscriptions.push(services.registLanguageClient(client));
}
