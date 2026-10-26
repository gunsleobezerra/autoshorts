#!/usr/bin/env node
/**
 * Wrapper de `vitest` para os scripts do package.json.
 *
 *   npm test                      # roda a suíte uma vez (vitest run)
 *   npm test -- format            # roda apenas arquivos que casam com "format"
 *   npm run test watch            # modo watch (equivalente a `npm run test:watch`)
 *   npm run test:watch            # modo watch
 *
 * Motivo de existir: `npm run test watch` entrega `watch` como argumento
 * posicional, e o Vitest interpreta posicionais como filtro de nome de
 * arquivo. O resultado era `No test files found, exiting with code 1` mesmo
 * com a suíte toda verde. Aqui um `watch` isolado vira o modo watch; os
 * demais argumentos continuam sendo filtros.
 *
 * Também valida a versão do Node antes de subir o Vitest: o jsdom 30 depende
 * do undici 8, que exige Node >= 22.19 (usa `zlib.createZstdDecompress`). Em
 * Node 20 o Vitest morria com `TypeError: Cannot read properties of undefined
 * (reading 'bind')` dentro de `undici/lib/interceptor/decompress.js`.
 */
import { spawnSync } from 'node:child_process';
import fs from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const ROOT = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
const VITEST = path.join(ROOT, 'node_modules', 'vitest', 'vitest.mjs');

if (!fs.existsSync(VITEST)) {
  console.error('Vitest não encontrado. Rode `npm install` antes de `npm test`.');
  process.exit(1);
}

/** Compara duas versões `x.y.z`; devolve negativo se `a` for mais antiga. */
function compareVersions(a, b) {
  const left = a.split('.').map(Number);
  const right = b.split('.').map(Number);
  for (let index = 0; index < 3; index += 1) {
    const diff = (left[index] || 0) - (right[index] || 0);
    if (diff !== 0) return diff;
  }
  return 0;
}

// `engines.node` é a fonte da verdade; `>=22.19.0` vira o mínimo comparável.
const manifest = JSON.parse(fs.readFileSync(path.join(ROOT, 'package.json'), 'utf8'));
const required = /([0-9]+\.[0-9]+\.[0-9]+)/.exec(manifest.engines?.node ?? '')?.[1];
if (required && compareVersions(process.versions.node, required) < 0) {
  console.error(
    `Node ${process.versions.node} não é suportado: este projeto exige >= ${required}.\n` +
      'O jsdom 30 usa undici 8, que falha em versões anteriores.\n' +
      'Atualize o Node (ex.: `nvm use 22`) e rode de novo.',
  );
  process.exit(1);
}

const raw = process.argv.slice(2);
const watch = raw.includes('watch');
const args = raw.filter((arg) => arg !== 'watch');

const result = spawnSync(
  process.execPath,
  [VITEST, ...(watch ? [] : ['run']), ...args],
  { cwd: ROOT, stdio: 'inherit' },
);

if (result.error) {
  console.error(result.error.message);
  process.exit(1);
}

process.exit(result.status ?? 1);
