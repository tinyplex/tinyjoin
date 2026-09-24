import {
  NodeBreadcrumbs,
  NodeNavigation,
  NodeSection,
  usePageNode,
  useRootNode,
} from 'tinydocs';

export const MainInner = () => {
  const node = usePageNode();
  const root = useRootNode();

  return node === root ? null : (
    <>
      <nav aria-label="Documentation">
        <ul>
          <NodeNavigation node={root} />
        </ul>
      </nav>
      <article id="content" tabIndex={-1}>
        <nav className="breadcrumbs" aria-label="Breadcrumbs">
          <ul>
            <NodeBreadcrumbs node={root} />
          </ul>
        </nav>
        <NodeSection node={node} />
      </article>
      <aside aria-label="On this page" />
    </>
  );
};
