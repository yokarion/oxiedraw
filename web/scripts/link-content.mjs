// Create web/content as a link to the top-level docs/ folder.
//
// Docus reads content from web/content, but the pages live in top-level docs/.
// A committed git symlink breaks on Windows checkouts (it materializes as a plain
// text file), so instead we (re)create the link here - a real symlink on POSIX,
// a directory junction on Windows (junctions need no admin rights). Idempotent
// and safe to run before every dev/build.
import { existsSync, rmSync, symlinkSync } from 'node:fs'
import { dirname, join, resolve } from 'node:path'
import { fileURLToPath } from 'node:url'

const webDir = resolve(dirname(fileURLToPath(import.meta.url)), '..')
const link = join(webDir, 'content')

// A working link already resolves docs/index.md - nothing to do.
if (existsSync(join(link, 'index.md'))) process.exit(0)

// Clear a stale entry (broken link, or a Windows checkout's text file).
rmSync(link, { recursive: true, force: true })

if (process.platform === 'win32') {
  symlinkSync(resolve(webDir, '..', 'docs'), link, 'junction')
} else {
  symlinkSync('../docs', link, 'dir')
}

console.log('Linked web/content -> ../docs')
