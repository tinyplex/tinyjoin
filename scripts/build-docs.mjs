import {withSiteBuild} from './build-site.mjs';

await withSiteBuild((build) =>
  build(process.argv[2] ?? 'docs', process.argv[3] ?? 'dist/@types'),
);
