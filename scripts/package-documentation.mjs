import {mkdir, readFile, readdir, writeFile} from 'node:fs/promises';
import {dirname, posix, resolve} from 'node:path';

// Keep installed guidance self-contained without publishing a second set of
// authored docs. Guide links stay local; links to demos and website API pages
// remain available online. The documented declarations are packaged separately.
export async function getPackageDocumentation(root) {
  const directory = resolve(root, 'site/guides');
  const files = (await readdir(directory)).filter((file) => file.endsWith('.md')).sort();
  const guides = files.map((file) => ({
    file,
    slug: file.replace(/^\d+_/, '').replace(/\.md$/, '').replaceAll('_', '-'),
  }));
  const destinations = new Map(guides.map(({slug}) => [
    slug === 'index' ? '/guides/' : `/guides/${slug}/`,
    `docs/guides/${slug}.md`,
  ]));
  const sizes = JSON.parse(await readFile(resolve(root, 'site/data/sizes.json'), 'utf8'));
  const documents = {};
  for (const {file, slug} of guides) {
    let source = await readFile(resolve(directory, file), 'utf8');
    for (const [group, {gzipLabel}] of Object.entries(sizes)) {
      source = source.replaceAll(`{{sizes.${group}.gzip}}`, gzipLabel);
    }
    const targets = [`docs/guides/${slug}.md`];
    if (slug === 'sql-compatibility') targets.push('docs/sql.md');
    if (slug === 'agents') targets.push('agents.md');
    for (const target of targets) {
      documents[target] = source.replaceAll(
        /(\]\(|(?:href|src)=["'])((?:https:\/\/tinyjoin\.org)?\/(?!\/)[^)\s"']*)/g,
        (_match, prefix, href) => {
          const url = new URL(href, 'https://tinyjoin.org');
          const local = destinations.get(url.pathname);
          return prefix + (local
            ? posix.relative(posix.dirname(target), local) + url.hash
            : url.href);
        },
      );
    }
  }
  return documents;
}

export async function writePackageDocumentation(root, packageDirectory) {
  for (const [path, contents] of Object.entries(await getPackageDocumentation(root))) {
    const target = resolve(packageDirectory, path);
    await mkdir(dirname(target), {recursive: true});
    await writeFile(target, contents);
  }
}
