import { workspace } from 'coc.nvim';

describe('coc-make', () => {
  it('should have correct configuration properties', () => {
    const packageJson = require('../package.json');
    const config = packageJson.contributes.configuration.properties;

    expect(config['make.enable']).toBeDefined();
    expect(config['make.enable'].type).toBe('boolean');
    expect(config['make.enable'].default).toBe(true);

    expect(config['make.serverPath']).toBeDefined();
    expect(config['make.serverPath'].type).toBe('string');
    expect(config['make.serverPath'].default).toBe('makefile-lsp');
  });
});
