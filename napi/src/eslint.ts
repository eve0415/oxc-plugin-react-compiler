import type { Linter, Rule } from 'eslint';

import { allRules, mapSeverityToESLint, recommendedRules } from './eslint-rules';

const meta = {
  name: 'oxc-react-compiler',
  version: '0.1.0',
};

const rules: Record<string, Rule.RuleModule> = Object.fromEntries(Object.entries(allRules).map(([name, { rule }]) => [name, rule]));

const configs: Record<string, Linter.Config> = {
  recommended: {
    plugins: {
      'oxc-react-compiler': { meta, rules },
    },
    rules: Object.fromEntries(
      Object.entries(recommendedRules).map(([name, config]) => [`oxc-react-compiler/${name}`, mapSeverityToESLint(config.severity)]),
    ),
  },
  all: {
    plugins: {
      'oxc-react-compiler': { meta, rules },
    },
    rules: Object.fromEntries(Object.entries(allRules).map(([name, config]) => [`oxc-react-compiler/${name}`, mapSeverityToESLint(config.severity)])),
  },
};

export { configs, meta, rules };
export default { meta, rules, configs } as { meta: typeof meta; rules: typeof rules; configs: typeof configs };
