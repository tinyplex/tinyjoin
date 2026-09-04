import type {Node} from 'tinydocs';
import {Markdown} from 'tinydocs';

export const Home = ({node}: {node: Node}) => (
  <article id="home" tabIndex={-1}>
    <em>
      <img
        src="/favicon.svg"
        alt="Large TinyJoin logo"
        width="100%"
        height="100%"
      />
    </em>
    {node.summary ? <Markdown markdown={node.summary} html={true} /> : null}
    {node.body ? <Markdown markdown={node.body} html={true} /> : null}
  </article>
);
