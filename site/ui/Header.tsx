import {NodeChildren, usePageNode, useRootNode} from 'tinydocs';

export const Header = () => {
  const root = useRootNode();

  return (
    <header>
      <a
        className="wordmark"
        href="/"
        aria-current={usePageNode() === root ? 'page' : undefined}
      >
        <img src="/favicon.svg" alt="TinyJoin logo" />
        <span>
          Tiny<em>Join</em>
        </span>
      </a>
      <nav aria-label="Primary">
        <ul>
          <NodeChildren node={root} />
          <li>
            <a href="https://github.com/tinyplex/tinyjoin">GitHub</a>
          </li>
        </ul>
      </nav>
      <button
        id="dark"
        className="auto"
        type="button"
        aria-label="Color theme: automatic; activate for dark"
      />
    </header>
  );
};
