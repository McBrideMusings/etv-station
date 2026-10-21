#!/usr/bin/env node
// Check every in-site anchor link under docs/: `](#slug)` and `](/page#slug)`.
// VitePress's dead-link check covers pages only, not anchors, so a renamed
// heading rots its links without failing `docs:build`. This script has no
// dependencies: it reimplements VitePress's heading slugs (@mdit-vue/shared
// `slugify`) and checks each anchor against the target page's headings.
//
// Usage: node tools/check-doc-links.mjs   (exit 1 if any anchor is dead)

import { readdirSync, readFileSync, existsSync } from 'node:fs'
import { join, dirname, relative, resolve } from 'node:path'
import { fileURLToPath } from 'node:url'

const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), '..')
const DOCS = join(ROOT, 'docs')

const rControl = /[\u0000-\u001f]/g
const rSpecial = /[\s~`!@#$%^&*()\-_+=[\]{}|\;:"'“”‘’<>,.?/]+/g
const rCombining = /[̀-ͯ]/g

function slugify(str) {
  return str
    .normalize('NFKD')
    .replace(rCombining, '')
    .replace(rControl, '')
    .replace(rSpecial, '-')
    .replace(/-{2,}/g, '-')
    .replace(/^-+|-+$/g, '')
    .replace(/^(\d)/, '_$1')
    .toLowerCase()
}

function walk(dir) {
  const out = []
  for (const e of readdirSync(dir, { withFileTypes: true })) {
    if (e.name === 'node_modules' || e.name === '.vitepress' || e.name.startsWith('.')) continue
    const p = join(dir, e.name)
    if (e.isDirectory()) out.push(...walk(p))
    else if (e.name.endsWith('.md')) out.push(p)
  }
  return out
}

// Blank out fenced code so `#` lines and example links inside it don't count.
function stripFences(text) {
  return text.replace(/^(```|~~~)[^\n]*\n[\s\S]*?^\1[ \t]*$/gm, (m) => m.replace(/[^\n]/g, ''))
}

function headingText(raw) {
  return raw
    .replace(/\s*\{#([^}]+)\}\s*$/, '')
    .replace(/!?\[([^\]]*)\]\([^)]*\)/g, '$1')
    .replace(/<[^>]+>/g, '')
    .replace(/[`*_]/g, (c) => (c === '`' ? '' : c === '*' ? '' : c))
    .trim()
}

function anchorsOf(text) {
  const ids = new Set()
  const seen = new Map()
  for (const m of stripFences(text).matchAll(/^#{1,6}[ \t]+(.+?)[ \t]*#*[ \t]*$/gm)) {
    const custom = m[1].match(/\{#([^}]+)\}\s*$/)
    let id = custom ? custom[1] : slugify(headingText(m[1]))
    const n = seen.get(id) ?? 0
    seen.set(id, n + 1)
    ids.add(n === 0 ? id : `${id}-${n}`)
  }
  for (const m of text.matchAll(/\bid="([^"]+)"/g)) ids.add(m[1])
  return ids
}

// Map a link target path (`/PRD`, `/adr/`, `./x`) to its markdown file.
function resolvePage(fromFile, target) {
  let base
  if (target.startsWith('/')) base = join(DOCS, target)
  else base = resolve(dirname(fromFile), target)
  base = base.replace(/\.(md|html)$/, '')
  for (const c of [`${base}.md`, join(base, 'index.md')]) if (existsSync(c)) return c
  return null
}

const files = walk(DOCS)
const pages = new Map(files.map((f) => [f, readFileSync(f, 'utf8')]))
const anchors = new Map([...pages].map(([f, t]) => [f, anchorsOf(t)]))

let bad = 0
for (const [file, text] of pages) {
  const noCode = stripFences(text).replace(/`[^`\n]*`/g, (c) => ' '.repeat(c.length))
  const lines = noCode.split('\n')
  lines.forEach((line, i) => {
    for (const m of line.matchAll(/\]\(([^)\s]*)#([^)\s]+)\)/g)) {
      const [, path, frag] = m
      if (/^[a-z][a-z0-9+.-]*:/i.test(path)) continue
      const targetFile = path === '' ? file : resolvePage(file, path)
      const where = `${relative(ROOT, file)}:${i + 1}`
      if (!targetFile) {
        console.error(`${where}: link to missing page ${path}#${frag}`)
        bad++
      } else if (!anchors.get(targetFile)?.has(decodeURIComponent(frag))) {
        console.error(`${where}: no heading #${frag} in ${relative(ROOT, targetFile)}`)
        bad++
      }
    }
  })
}

if (bad) {
  console.error(`\n${bad} dead anchor link(s).`)
  process.exit(1)
}
console.log(`checked ${files.length} docs pages: all anchor links resolve.`)
