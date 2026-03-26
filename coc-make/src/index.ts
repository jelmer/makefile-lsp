import {
  ExtensionContext,
  LanguageClient,
  LanguageClientOptions,
  ServerOptions,
  services,
  workspace
} from 'coc.nvim';

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
