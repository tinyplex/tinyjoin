import {Markdown, usePageNode} from 'tinydocs';

export const MarkdownPage = () => {
  const {summary, body} = usePageNode();
  return (
    <Markdown
      markdown={[summary, body].filter(Boolean).join('\n\n')}
      html={true}
      skipCode={true}
    />
  );
};
