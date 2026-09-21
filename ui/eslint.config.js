import js from '@eslint/js'
import globals from 'globals'
import reactHooks from 'eslint-plugin-react-hooks'
import reactRefresh from 'eslint-plugin-react-refresh'
import jsxA11y from 'eslint-plugin-jsx-a11y'
import tseslint from 'typescript-eslint'

/**
 * 前端的第三样检查（另外两样是 `tsc -b` 和 `vite build`）。
 *
 * 真正要它的是 **jsx-a11y** 和 **react-hooks**：这两类问题 TypeScript 一个都看不出来——
 * onClick 挂在 div 上、`<a>` 里套 `<button>`、图标按钮没名字、依赖数组漏了东西，全都类型正确。
 * 接它之前，代码里那几行 `eslint-disable-next-line react-hooks/exhaustive-deps` 是空转的注释：
 * 仓库里根本没有 eslint，没有任何东西在检查它们。
 */
export default tseslint.config(
  { ignores: ['dist', 'node_modules'] },
  {
    files: ['**/*.{ts,tsx}'],
    extends: [js.configs.recommended, ...tseslint.configs.recommended, jsxA11y.flatConfigs.recommended],
    languageOptions: {
      ecmaVersion: 2022,
      globals: { ...globals.browser, __APP_VERSION__: 'readonly' },
    },
    plugins: { 'react-hooks': reactHooks, 'react-refresh': reactRefresh },
    rules: {
      /*
       * hooks 只开经典的两条。
       *
       * 插件 7.x 的 recommended 里还塞了一整套 **React Compiler** 的规则（refs / purity /
       * set-state-in-effect / incompatible-library）。我们没开编译器，而这些规则会把本项目
       * 大量「故意为之」的命令式写法判成错：量完布局再 setState（虚拟列表要实测行高）、
       * render 里读 ref 锁住一次查询参数、TanStack Virtual 的返回值。开了就是逼着为一个
       * 没在用的编译器重写这些地方，所以只留下真正在防 bug 的那两条。
       */
      'react-hooks/rules-of-hooks': 'error',
      'react-hooks/exhaustive-deps': 'warn',
      'react-refresh/only-export-components': 'off',
      // 用 `_` 开头表示「知道没用到」
      '@typescript-eslint/no-unused-vars': ['error', { argsIgnorePattern: '^_', varsIgnorePattern: '^_' }],
      // 中文排版里的全角空格是正常字符，别当成不可见的脏字符报错
      'no-irregular-whitespace': ['error', { skipStrings: true, skipTemplates: true, skipJSXText: true, skipComments: true }],
    },
    settings: {
      // 自己封的表单控件（都只是 input / select 加一身类名），label 包着它们算关联上了
      'jsx-a11y': { components: { Input: 'input', Select: 'select' } },
    },
  },
)
