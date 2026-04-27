import { rebuildIndex, reclassifyLedger } from '@relayburn/ledger';

import { reingestMissingContent } from '../ingest.js';
import { formatInt } from '../format.js';
import type { ParsedArgs } from '../args.js';

const REBUILD_HELP = `burn rebuild — rebuild derived ledger artifacts

Usage:
  burn rebuild --index
  burn rebuild --reclassify [--force]
  burn rebuild --content
  burn rebuild --index --reclassify [--force]

Flags:
  --index       rebuild the sidecar index
  --reclassify  re-run the activity classifier on every ledger turn
  --force       with --reclassify, overwrite activity even if already set
  --content     re-parse source session files to populate missing content
                sidecars. Skips sessions that already have content on disk.
                Does not touch cursors or existing ledger rows.

`;

export async function runRebuild(args: ParsedArgs): Promise<number> {
  const doIndex = args.flags['index'] === true;
  const doReclassify = args.flags['reclassify'] === true;
  const doContent = args.flags['content'] === true;
  const force = args.flags['force'] === true;

  if (!doIndex && !doReclassify && !doContent) {
    process.stdout.write(REBUILD_HELP);
    return 0;
  }

  const lines: string[] = [];

  if (doReclassify) {
    const report = await reclassifyLedger({ force });
    const unchanged = report.processed - report.changed;
    lines.push(
      `reclassified ${formatInt(report.processed)} of ${formatInt(report.scanned)} turns` +
        ` (${formatInt(report.skipped)} skipped, already classified)`,
    );
    lines.push(
      `  ${formatInt(report.changed)} ended up with a different activity label,` +
        ` ${formatInt(unchanged)} unchanged`,
    );
    if (report.changed > 0) {
      const changes = Object.entries(report.changedByCategory).sort((a, b) => b[1] - a[1]);
      for (const [cat, n] of changes) {
        lines.push(`    → ${cat}: ${formatInt(n)}`);
      }
    }
  }

  if (doContent) {
    const r = await reingestMissingContent();
    lines.push(
      `reingested content for ${formatInt(r.reingestedSessions)} sessions` +
        ` (${formatInt(r.scannedFiles)} files scanned,` +
        ` ${formatInt(r.skippedExisting)} already had content,` +
        ` ${formatInt(r.appendedContent)} records appended,` +
        ` ${formatInt(r.failed)} failed)`,
    );
  }

  // Rebuild index after reclassify — ids/fingerprints are unchanged by
  // reclassification today, but doing it together gives users one command for
  // "fix up my ledger" and guards against future changes where they might.
  if (doIndex) {
    const { ids, content } = await rebuildIndex();
    lines.push(
      `rebuilt ledger index: ${formatInt(ids)} id hashes, ${formatInt(content)} content fingerprints`,
    );
  }

  process.stdout.write(lines.join('\n') + '\n');
  return 0;
}
