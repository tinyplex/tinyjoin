import {Markdown, usePageNode} from 'tinydocs';
import {getBenchmarks} from '../benchmarks.ts';

// Public Markdown copies, such as the README, show benchmark charts as tables,
// since hosts like GitHub strip the styles the website draws them with.
export const MarkdownPage = () => {
  const {summary, body} = usePageNode();
  return (
    <Markdown
      markdown={getBenchmarks().renderTables(
        [summary, body].filter(Boolean).join('\n\n'),
      )}
      html={true}
      skipCode={true}
    />
  );
};
