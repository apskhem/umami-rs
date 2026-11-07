/**
 * Rewrites an upstream Umami checkout so `next build` produces a static export
 * of the dashboard (no Node server needed at runtime).
 *
 *   pnpm exec tsx prepare.mts <umami-checkout>
 *
 * Upstream's pages are thin server components that only await their route
 * params and render a client component, so the whole app router tree is
 * replaced by one exported page (`[[...slug]]`, prerendered for `/` only)
 * whose client-side `StaticRouter` matches the URL against a route table
 * generated here from upstream's `page.tsx` / `layout.tsx` files, and applies
 * upstream's page redirects and rewrites from `next.config.ts`.
 *
 * Every transformation is checked: if an upstream bump adds server-side logic
 * this script doesn't know how to strip, it fails rather than ship it.
 */

import fs from 'node:fs';
import path from 'node:path';
import { pathToFileURL } from 'node:url';

const root = path.resolve(process.argv[2] ?? '.');
const app = path.join(root, 'src/app');

// Server-only route trees: the API (served by umami-rs), link/pixel redirects,
// legacy report rewrites and the MCP endpoint.
const SERVER_DIRS = ['api', '(collect)', '(compat)', 'mcp'];

type Segment =
  | { kind: 'static'; value: string }
  | { kind: 'param' | 'catchAll' | 'optionalCatchAll'; name: string };

interface Layout {
  file: string;
  dir: string;
  params: string[];
}

interface Route {
  file: string;
  dir: string;
  segments: Segment[];
  layouts: Layout[];
}

function fail(message: string): never {
  console.error(`prepare: ${message}`);
  process.exit(1);
}

function rel(file: string) {
  return path.relative(app, file).split(path.sep).join('/');
}

function parseSegment(name: string): Segment | null {
  if (/^\(.*\)$/.test(name)) return null;
  let m = name.match(/^\[\[\.\.\.(\w+)\]\]$/);
  if (m) return { kind: 'optionalCatchAll', name: m[1] };
  m = name.match(/^\[\.\.\.(\w+)\]$/);
  if (m) return { kind: 'catchAll', name: m[1] };
  m = name.match(/^\[(\w+)\]$/);
  if (m) return { kind: 'param', name: m[1] };
  if (/[[\]@()]/.test(name)) fail(`unsupported route segment ${name}`);
  return { kind: 'static', value: name };
}

function paramNames(segments: Segment[]) {
  return segments.flatMap(s => (s.kind === 'static' ? [] : [s.name]));
}

/** Strips the server-side parts of an upstream page or layout module. */
function clientify(file: string) {
  let src = fs.readFileSync(file, 'utf8');

  // Existence checks that query the database (links, pixels, websites). The
  // client components behind them fetch the record themselves.
  src = src.replace(/^import \{[^}]*\} from '@\/queries\/prisma';\n/m, '');
  src = src.replace(
    /\n\s*const (\w+) = await get\w+\(\w+\);\n\s*if \(![^{]*\{\n\s*return null;\n\s*\}\n/g,
    '\n',
  );
  // Local helpers that are async without awaiting anything (the test
  // console's `ENABLE_TEST_CONSOLE` check, which is never set in the browser).
  for (const [helper, name, body] of src.matchAll(/async function (\w+)\(\) \{([^}]*)\}/g)) {
    if (/\bawait\b/.test(body)) continue;
    src = src.replace(helper, helper.replace(/^async /, ''));
    src = src.replaceAll(`await ${name}(`, `${name}(`);
  }
  src = src.replace(/export default async function/g, 'export default function');
  src = src.replace(/await params\b/g, 'params');
  src = src.replace(/^export const dynamic = .*\n/m, '');
  src = src.replace(/params: Promise<([^>]*)>/g, 'params: $1');

  for (const forbidden of [/\bawait\b/, /\basync\b/, /@\/queries\/prisma/, /next\/headers/]) {
    if (forbidden.test(src)) fail(`${rel(file)}: server-side code left after rewrite (${forbidden})`);
  }
  fs.writeFileSync(file, src);
}

function walk(dir: string, segments: Segment[], layouts: Layout[], routes: Route[]) {
  const entries = fs.readdirSync(dir, { withFileTypes: true });
  const names = new Set(entries.map(e => e.name));

  if (dir !== app && names.has('layout.tsx')) {
    const file = path.join(dir, 'route-layout.tsx');
    fs.renameSync(path.join(dir, 'layout.tsx'), file);
    clientify(file);
    layouts = [...layouts, { file, dir: rel(dir) || '.', params: paramNames(segments) }];
  }
  if (names.has('page.tsx')) {
    const file = path.join(dir, 'route-page.tsx');
    fs.renameSync(path.join(dir, 'page.tsx'), file);
    clientify(file);
    routes.push({ file, dir: rel(dir) || '.', segments, layouts });
  }
  for (const special of ['route.ts', 'template.tsx', 'loading.tsx', 'error.tsx', 'default.tsx']) {
    if (names.has(special)) fail(`${rel(path.join(dir, special))}: unsupported route file`);
  }

  for (const entry of entries) {
    if (!entry.isDirectory()) continue;
    const child = path.join(dir, entry.name);
    // Intercepting routes (`@modal/(.)sessions/...`) open a page as a modal
    // during client navigation; the static build shows the page instead.
    if (entry.name.startsWith('@')) {
      fs.rmSync(child, { recursive: true });
      continue;
    }
    const segment = parseSegment(entry.name);
    walk(child, segment ? [...segments, segment] : segments, layouts, routes);
  }
}

/** Next's route specificity: static before dynamic before catch-all. */
function rank(segment: Segment) {
  return { static: 0, param: 1, catchAll: 2, optionalCatchAll: 3 }[segment.kind];
}

function compareRoutes(a: Route, b: Route) {
  for (let i = 0; i < Math.max(a.segments.length, b.segments.length); i++) {
    const x = a.segments[i];
    const y = b.segments[i];
    if (!x || !y) return x ? 1 : -1;
    if (rank(x) !== rank(y)) return rank(x) - rank(y);
  }
  return 0;
}

function escapeRegex(s: string) {
  return s.replace(/[.*+?^${}()|[\]\\]/g, '\\$&');
}

function routeRegex(segments: Segment[]) {
  if (!segments.length) return '^/$';
  const parts = segments.map(s => {
    switch (s.kind) {
      case 'static':
        return `/${escapeRegex(s.value)}`;
      case 'param':
        return '/([^/]+)';
      case 'catchAll':
        return '/(.+)';
      case 'optionalCatchAll':
        return '(?:/(.+))?';
    }
  });
  return `^${parts.join('')}$`;
}

/** Converts a path-to-regexp source as used in upstream's next.config.ts. */
function sourceRegex(source: string) {
  const names: string[] = [];
  const body = source.replace(/\/:(\w+)(\*|\+)?|[.*+?^${}()|[\]\\]/g, (m, name, modifier) => {
    if (!name) return `\\${m}`;
    names.push(name);
    if (modifier === '*') return '(?:/(.*))?';
    if (modifier === '+') return '/(.+)';
    return '/([^/]+)';
  });
  return { regex: `^${body}$`, names };
}

async function loadNextConfig() {
  const mod = await import(pathToFileURL(path.join(root, 'next.config.ts')).href);
  const config = mod.default?.default ?? mod.default;
  const rewrites = await config.rewrites();
  const redirects = await config.redirects();
  const list = Array.isArray(rewrites)
    ? rewrites
    : [...rewrites.beforeFiles, ...rewrites.afterFiles, ...rewrites.fallback];

  // Only rewrites between dashboard pages; the rest point at API routes,
  // which umami-rs serves itself.
  const pageRewrites = list.filter(
    (r: any) => !/^\/(api|compat)\//.test(r.destination) && !/^https?:/.test(r.destination),
  );
  const unexpected = pageRewrites.filter((r: any) => !r.source.startsWith('/teams/'));
  if (unexpected.length) fail(`unhandled rewrites: ${JSON.stringify(unexpected)}`);

  const toRule = (r: any) => ({ ...sourceRegex(r.source), destination: r.destination });
  return { redirects: redirects.map(toRule), rewrites: pageRewrites.map(toRule) };
}

function patchRootLayout() {
  const file = path.join(app, 'layout.tsx');
  let src = fs.readFileSync(file, 'utf8');
  // `metadataBase` comes from the request's host header; a static page has none.
  src = src.replace(/^import \{ headers \} from 'next\/headers';\n/m, '');
  src = src.replace(/^import \{ getBaseUrl \} from '@\/lib\/get-base-url';\n/m, '');
  src = src.replace(
    /export async function generateMetadata\(\)[\s\S]*$/,
    `export const metadata: Metadata = {
  title: {
    template: '%s | Umami',
    default: 'Umami',
  },
};
`,
  );
  if (/headers\(|generateMetadata/.test(src)) fail('layout.tsx: could not remove generateMetadata');
  fs.writeFileSync(file, src);
}

function patchNextConfig() {
  const file = path.join(root, 'next.config.ts');
  let src = fs.readFileSync(file, 'utf8');
  if (!src.includes('export default withNextIntl(')) fail('next.config.ts: unexpected shape');
  src = src.replace('export default withNextIntl(', 'const config = withNextIntl(');
  src += `
const {
  headers: _headers,
  rewrites: _rewrites,
  redirects: _redirects,
  ...staticConfig
} = config as any;

export default {
  ...staticConfig,
  output: 'export',
  images: { unoptimized: true },
};
`;
  fs.writeFileSync(file, src);
}

function generateRouter(routes: Route[], rules: Awaited<ReturnType<typeof loadNextConfig>>) {
  const modules = new Map<string, string>();
  const ident = (file: string) => {
    if (!modules.has(file)) modules.set(file, `m${modules.size}`);
    return modules.get(file)!;
  };
  const importPath = (file: string) => `./${rel(file).replace(/\.tsx$/, '')}`;

  const table = routes
    .sort(compareRoutes)
    .map(r => {
      const layouts = r.layouts
        .map(l => `{ mod: ${ident(l.file)}, id: ${JSON.stringify(l.dir)}, params: ${JSON.stringify(l.params)} }`)
        .join(', ');
      return `  {
    regex: /${routeRegex(r.segments).replace(/\//g, '\\/')}/,
    params: ${JSON.stringify(r.segments.filter(s => s.kind !== 'static').map(s => [s.name, s.kind]))},
    id: ${JSON.stringify(r.dir)},
    page: ${ident(r.file)},
    layouts: [${layouts}],
  },`;
    })
    .join('\n');

  const rule = (r: { regex: string; names: string[]; destination: string }) =>
    `  { regex: /${r.regex.replace(/\//g, '\\/')}/, names: ${JSON.stringify(r.names)}, destination: ${JSON.stringify(r.destination)} },`;

  const imports = [...modules]
    .map(([file, id]) => `import * as ${id} from ${JSON.stringify(importPath(file))};`)
    .join('\n');

  return `'use client';
// @generated by tools/dashboard/prepare.mts from upstream's app directory.
import { redirect, usePathname } from 'next/navigation';
import { createElement, useEffect, useState, type ReactNode } from 'react';
import NotFound from './not-found';
import { metadata as rootMetadata } from './layout-metadata';
${imports}

type Module = { default: (props: any) => ReactNode; metadata?: { title?: any } };
type Rule = { regex: RegExp; names: string[]; destination: string };

const routes: {
  regex: RegExp;
  params: [string, string][];
  id: string;
  page: Module;
  layouts: { mod: Module; id: string; params: string[] }[];
}[] = [
${table}
];

const redirects: Rule[] = [
${rules.redirects.map(rule).join('\n')}
];

const rewrites: Rule[] = [
${rules.rewrites.map(rule).join('\n')}
];

function applyRule(rules: Rule[], path: string) {
  for (const { regex, names, destination } of rules) {
    const m = path.match(regex);
    if (m) {
      return destination.replace(/:(\\w+)\\*?/g, (_, name) => m[names.indexOf(name) + 1] ?? '');
    }
  }
  return null;
}

/** Next's title resolution: a segment's \`template\` applies to its children. */
function resolveTitle(chain: (Module['metadata'] | undefined)[]) {
  let template: string | undefined;
  let title = '';
  for (const metadata of chain) {
    const value = metadata?.title;
    if (typeof value === 'string') {
      title = template ? template.replace('%s', value) : value;
    } else if (value) {
      if (value.absolute) title = value.absolute;
      else if (value.default) title = value.default;
      if (value.template) template = value.template;
    }
  }
  return title;
}

function pick(params: Record<string, any>, names: string[]) {
  return Object.fromEntries(names.map(name => [name, params[name]]));
}

function match(pathname: string) {
  for (const route of routes) {
    const m = pathname.match(route.regex);
    if (!m) continue;
    const params: Record<string, any> = {};
    route.params.forEach(([name, kind], i) => {
      const value = m[i + 1];
      if (kind === 'param') params[name] = decodeURIComponent(value);
      else if (value !== undefined) params[name] = value.split('/').map(decodeURIComponent);
    });
    return { route, params };
  }
  return null;
}

export function StaticRouter() {
  const pathname = usePathname();
  const [mounted, setMounted] = useState(false);
  const target = applyRule(redirects, pathname);
  const matched = match(applyRule(rewrites, pathname) ?? pathname);
  const title = matched
    ? resolveTitle([rootMetadata, ...matched.route.layouts.map(l => l.mod.metadata), matched.route.page.metadata])
    : resolveTitle([rootMetadata]);

  // The export is prerendered for \`/\` only, so render nothing until the
  // browser's URL is known, to avoid a hydration mismatch.
  useEffect(() => setMounted(true), []);
  useEffect(() => {
    document.title = title;
  }, [title]);

  if (!mounted) return null;
  if (target) redirect(target);
  if (!matched) return createElement(NotFound);

  const { route, params } = matched;
  const json = (value: unknown) => JSON.stringify(value);
  let element = createElement(route.page.default, { key: route.id + json(params), params });
  for (const layout of [...route.layouts].reverse()) {
    const own = pick(params, layout.params);
    element = createElement(
      layout.mod.default,
      { key: layout.id + json(own), params: own, modal: null },
      element,
    );
  }
  return element;
}
`;
}

async function main() {
  if (!fs.existsSync(path.join(app, 'layout.tsx'))) fail(`no src/app/layout.tsx under ${root}`);
  const rules = await loadNextConfig();

  for (const dir of SERVER_DIRS) fs.rmSync(path.join(app, dir), { recursive: true, force: true });

  const routes: Route[] = [];
  walk(app, [], [], routes);
  if (!routes.length) fail('no pages found');

  patchRootLayout();
  // The router needs the root layout's metadata, but importing layout.tsx
  // itself would pull the server layout into the client bundle.
  const rootSrc = fs.readFileSync(path.join(app, 'layout.tsx'), 'utf8');
  const metadata = rootSrc.match(/export const metadata: Metadata = \{[\s\S]*?\n\};\n/)?.[0];
  if (!metadata) fail('layout.tsx: no metadata export');
  fs.writeFileSync(path.join(app, 'layout-metadata.ts'), `import type { Metadata } from 'next';\n\n${metadata}`);

  fs.writeFileSync(path.join(app, 'StaticRouter.tsx'), generateRouter(routes, rules));
  fs.mkdirSync(path.join(app, '[[...slug]]'));
  fs.writeFileSync(
    path.join(app, '[[...slug]]/page.tsx'),
    `import { StaticRouter } from '../StaticRouter';

export const dynamicParams = false;

export function generateStaticParams() {
  return [{ slug: [] }];
}

export default function () {
  return <StaticRouter />;
}
`,
  );

  patchNextConfig();

  // Route manifest for umami-server (`dashboard.rs`): the `BASE_PATH` the build
  // is for, which paths are pages (served the app shell), and upstream's
  // redirects and rewrites, in precedence order.
  fs.writeFileSync(
    path.join(root, 'public/routes.json'),
    `${JSON.stringify(
      {
        basePath: process.env.BASE_PATH || '',
        routes: routes.map(r => routeRegex(r.segments)),
        redirects: rules.redirects,
        rewrites: rules.rewrites,
      },
      null,
      2,
    )}\n`,
  );

  console.log(`prepare: ${routes.length} pages, ${rules.redirects.length} redirects, ${rules.rewrites.length} rewrites`);
}

await main();
