#!/usr/bin/env node
import { readdir, readFile, stat } from 'node:fs/promises';
import path from 'node:path';

const SOURCES = new Set(['auto', 'claude', 'codex', 'opencode']);

async function main() {
  const args = parseArgs(process.argv.slice(2));
  if (args.help) {
    printHelp();
    return;
  }
  if (args.paths.length === 0) {
    printHelp();
    process.exitCode = 2;
    return;
  }

  const { reader, analyze } = await loadBuiltPackages();
  const candidates = await discoverCandidates(args.paths, args.source);
  if (candidates.length === 0) {
    throw new Error('No Claude, Codex, or OpenCode session files matched the input paths.');
  }

  const heuristic = [];
  const cl100k = [];
  const failures = [];
  for (const candidate of candidates) {
    try {
      const heuristicResult = await parseCandidate(reader, candidate, 'heuristic');
      const cl100kResult = await parseCandidate(reader, candidate, 'cl100k');
      heuristic.push(heuristicResult);
      cl100k.push(cl100kResult);
    } catch (err) {
      failures.push({
        file: candidate.file,
        source: candidate.source,
        error: err instanceof Error ? err.message : String(err),
      });
    }
  }

  const heuristicTurns = heuristic.flatMap((r) => r.turns);
  const heuristicUserTurns = heuristic.flatMap((r) => r.userTurns);
  const cl100kUserTurns = cl100k.flatMap((r) => r.userTurns);

  const tokenStats = compareTokenStats(heuristicUserTurns, cl100kUserTurns);
  const pricing = await analyze.loadBuiltinPricing();
  const attribution = compareAttribution({
    turns: heuristicTurns,
    heuristicUserTurns,
    cl100kUserTurns,
    pricing,
    attributeWaste: analyze.attributeWaste,
  });

  const report = {
    files: {
      matched: candidates.length,
      parsed: heuristic.length,
      failed: failures.length,
      bySource: countBy(candidates, (c) => c.source),
      failures,
    },
    tokens: tokenStats,
    attribution,
  };

  if (args.json) {
    process.stdout.write(`${JSON.stringify(report, null, 2)}\n`);
  } else {
    renderHuman(report);
  }
}

function parseArgs(argv) {
  const out = { source: 'auto', json: false, help: false, paths: [] };
  for (let i = 0; i < argv.length; i++) {
    const arg = argv[i];
    if (arg === '--help' || arg === '-h') {
      out.help = true;
    } else if (arg === '--') {
      continue;
    } else if (arg === '--json') {
      out.json = true;
    } else if (arg === '--source') {
      const source = argv[++i];
      if (!SOURCES.has(source)) {
        throw new Error(`--source must be one of: ${[...SOURCES].join(', ')}`);
      }
      out.source = source;
    } else if (arg.startsWith('--source=')) {
      const source = arg.slice('--source='.length);
      if (!SOURCES.has(source)) {
        throw new Error(`--source must be one of: ${[...SOURCES].join(', ')}`);
      }
      out.source = source;
    } else if (arg.startsWith('-')) {
      throw new Error(`Unknown flag: ${arg}`);
    } else {
      out.paths.push(arg);
    }
  }
  return out;
}

async function loadBuiltPackages() {
  try {
    const reader = await import('../packages/reader/dist/index.js');
    const analyze = await import('../packages/analyze/dist/index.js');
    return { reader, analyze };
  } catch (err) {
    const code = err && typeof err === 'object' ? err.code : undefined;
    if (code === 'ERR_MODULE_NOT_FOUND') {
      throw new Error('Built packages are missing. Run `pnpm run build` before this script.');
    }
    throw err;
  }
}

async function discoverCandidates(inputs, requestedSource) {
  const out = [];
  for (const input of inputs) {
    await visitPath(path.resolve(input), requestedSource, true, out);
  }
  const seen = new Set();
  return out.filter((candidate) => {
    const key = `${candidate.source}\0${candidate.file}`;
    if (seen.has(key)) return false;
    seen.add(key);
    return true;
  });
}

async function visitPath(filePath, requestedSource, explicit, out) {
  const s = await stat(filePath);
  if (s.isDirectory()) {
    const entries = await readdir(filePath, { withFileTypes: true });
    entries.sort((a, b) => a.name.localeCompare(b.name));
    for (const entry of entries) {
      if (entry.name === 'node_modules' || entry.name === 'dist' || entry.name === '.git') {
        continue;
      }
      await visitPath(path.join(filePath, entry.name), requestedSource, false, out);
    }
    return;
  }
  if (!s.isFile()) return;

  if (requestedSource === 'auto') {
    const source = await detectSource(filePath);
    if (source) out.push({ file: filePath, source });
    return;
  }

  if (explicit || likelySourcePath(filePath, requestedSource)) {
    out.push({ file: filePath, source: requestedSource });
  }
}

function likelySourcePath(filePath, source) {
  if (source === 'claude' || source === 'codex') return filePath.endsWith('.jsonl');
  if (source !== 'opencode' || !filePath.endsWith('.json')) return false;
  const parts = filePath.split(path.sep);
  return parts.includes('storage') && parts.includes('session');
}

async function detectSource(filePath) {
  if (filePath.endsWith('.jsonl')) {
    const line = await firstJsonLine(filePath);
    if (!line) return null;
    if (line.type === 'session_meta' || line.type === 'turn_context') return 'codex';
    if (line.type === 'response_item' || line.type === 'event_msg') return 'codex';
    if (line.type === 'assistant' || line.type === 'user' || line.type === 'system') {
      return 'claude';
    }
    return null;
  }

  if (!filePath.endsWith('.json')) return null;
  if (!likelySourcePath(filePath, 'opencode')) return null;
  try {
    const parsed = JSON.parse(await readFile(filePath, 'utf8'));
    if (parsed && typeof parsed === 'object' && typeof parsed.id === 'string') {
      return 'opencode';
    }
  } catch {
    return null;
  }
  return null;
}

async function firstJsonLine(filePath) {
  const text = await readFile(filePath, 'utf8');
  for (const line of text.split(/\r?\n/)) {
    const trimmed = line.trim();
    if (!trimmed) continue;
    try {
      return JSON.parse(trimmed);
    } catch {
      return null;
    }
  }
  return null;
}

async function parseCandidate(reader, candidate, tokenizer) {
  const options = { tokenizer, sessionPath: candidate.file };
  if (candidate.source === 'claude') {
    return reader.parseClaudeSession(candidate.file, options);
  }
  if (candidate.source === 'codex') {
    return reader.parseCodexSession(candidate.file, options);
  }
  if (candidate.source === 'opencode') {
    return reader.parseOpencodeSession(candidate.file, options);
  }
  throw new Error(`Unsupported source: ${candidate.source}`);
}

function compareTokenStats(heuristicUserTurns, cl100kUserTurns) {
  const clByKey = new Map(cl100kUserTurns.map((u) => [userTurnKey(u), u]));
  const blockObs = [];
  const turnObs = [];
  let missingUserTurns = 0;
  let blockMismatches = 0;

  for (const h of heuristicUserTurns) {
    const c = clByKey.get(userTurnKey(h));
    if (!c) {
      missingUserTurns++;
      continue;
    }
    const hTurnTokens = sumTokens(h.blocks);
    const cTurnTokens = sumTokens(c.blocks);
    turnObs.push({ heuristic: hTurnTokens, cl100k: cTurnTokens });

    if (h.blocks.length !== c.blocks.length) blockMismatches++;
    const n = Math.min(h.blocks.length, c.blocks.length);
    for (let i = 0; i < n; i++) {
      const hb = h.blocks[i];
      const cb = c.blocks[i];
      if (hb.kind !== cb.kind || hb.toolUseId !== cb.toolUseId) blockMismatches++;
      blockObs.push({
        kind: hb.kind,
        heuristic: hb.approxTokens,
        cl100k: cb.approxTokens,
      });
    }
  }

  return {
    userTurns: summarizeTokenObservations(turnObs),
    blocks: summarizeTokenObservations(blockObs),
    byBlockKind: {
      text: summarizeTokenObservations(blockObs.filter((o) => o.kind === 'text')),
      tool_result: summarizeTokenObservations(blockObs.filter((o) => o.kind === 'tool_result')),
    },
    missingUserTurns,
    blockMismatches,
  };
}

function compareAttribution({
  turns,
  heuristicUserTurns,
  cl100kUserTurns,
  pricing,
  attributeWaste,
}) {
  const heuristic = attributeWaste(turns, {
    pricing,
    userTurnsBySession: groupUserTurnsBySession(heuristicUserTurns),
  });
  const cl100k = attributeWaste(turns, {
    pricing,
    userTurnsBySession: groupUserTurnsBySession(cl100kUserTurns),
  });

  const hByCall = new Map(heuristic.attributions.map((a) => [attributionKey(a), a]));
  const cByCall = new Map(cl100k.attributions.map((a) => [attributionKey(a), a]));
  const callDeltas = [];
  let missingCalls = 0;
  for (const [key, h] of hByCall) {
    const c = cByCall.get(key);
    if (!c) {
      missingCalls++;
      continue;
    }
    const denom = Math.max(Math.abs(h.totalCost), Math.abs(c.totalCost), 1e-12);
    callDeltas.push({
      heuristic: h.totalCost,
      cl100k: c.totalCost,
      absDelta: Math.abs(c.totalCost - h.totalCost),
      relDelta: (c.totalCost - h.totalCost) / denom,
    });
  }

  return {
    calls: {
      count: callDeltas.length,
      missingCalls,
      heuristicTotal: heuristic.attributedTotal,
      cl100kTotal: cl100k.attributedTotal,
      delta: cl100k.attributedTotal - heuristic.attributedTotal,
      relativeDelta:
        Math.max(Math.abs(heuristic.attributedTotal), Math.abs(cl100k.attributedTotal)) > 0
          ? (cl100k.attributedTotal - heuristic.attributedTotal) /
            Math.max(Math.abs(heuristic.attributedTotal), Math.abs(cl100k.attributedTotal))
          : 0,
      medianAbsDollarDelta: median(callDeltas.map((d) => d.absDelta)),
      p95AbsDollarDelta: percentile(callDeltas.map((d) => d.absDelta), 0.95),
      medianAbsRelativeDelta: median(callDeltas.map((d) => Math.abs(d.relDelta))),
      p95AbsRelativeDelta: percentile(callDeltas.map((d) => Math.abs(d.relDelta)), 0.95),
      over5Pct: countOver(callDeltas.map((d) => Math.abs(d.relDelta)), 0.05),
    },
    ranking: compareToolRanking(heuristic.attributions, cl100k.attributions),
  };
}

function summarizeTokenObservations(obs) {
  const comparable = obs.filter((o) => o.cl100k > 0);
  const absDeviation = comparable.map((o) => Math.abs((o.heuristic - o.cl100k) / o.cl100k));
  return {
    count: obs.length,
    comparable: comparable.length,
    heuristicTokens: sum(obs.map((o) => o.heuristic)),
    cl100kTokens: sum(obs.map((o) => o.cl100k)),
    medianRatio: median(comparable.map((o) => o.heuristic / o.cl100k)),
    p95AbsDeviation: percentile(absDeviation, 0.95),
    over20Pct: countOver(absDeviation, 0.20),
  };
}

function compareToolRanking(heuristicAttributions, cl100kAttributions) {
  const hRanks = rankToolCosts(aggregateToolCosts(heuristicAttributions));
  const cRanks = rankToolCosts(aggregateToolCosts(cl100kAttributions));
  const names = new Set([
    ...hRanks.slice(0, 10).map((r) => r.tool),
    ...cRanks.slice(0, 10).map((r) => r.tool),
  ]);
  const rows = [...names].map((tool) => {
    const h = hRanks.find((r) => r.tool === tool);
    const c = cRanks.find((r) => r.tool === tool);
    return {
      tool,
      heuristicRank: h?.rank ?? null,
      cl100kRank: c?.rank ?? null,
      heuristicCost: h?.cost ?? 0,
      cl100kCost: c?.cost ?? 0,
      delta: (c?.cost ?? 0) - (h?.cost ?? 0),
    };
  });
  rows.sort(
    (a, b) =>
      Math.min(a.heuristicRank ?? 99, a.cl100kRank ?? 99) -
      Math.min(b.heuristicRank ?? 99, b.cl100kRank ?? 99),
  );
  return {
    top: rows,
    top10RankChanges: rows.filter(
      (r) =>
        r.heuristicRank !== null &&
        r.cl100kRank !== null &&
        r.heuristicRank <= 10 &&
        r.cl100kRank <= 10 &&
        r.heuristicRank !== r.cl100kRank,
    ).length,
  };
}

function aggregateToolCosts(attributions) {
  const out = new Map();
  for (const a of attributions) {
    out.set(a.toolName, (out.get(a.toolName) ?? 0) + a.totalCost);
  }
  return out;
}

function rankToolCosts(costs) {
  return [...costs.entries()]
    .sort((a, b) => b[1] - a[1] || a[0].localeCompare(b[0]))
    .map(([tool, cost], index) => ({ tool, cost, rank: index + 1 }));
}

function groupUserTurnsBySession(userTurns) {
  const out = new Map();
  for (const userTurn of userTurns) {
    const list = out.get(userTurn.sessionId) ?? [];
    list.push(userTurn);
    out.set(userTurn.sessionId, list);
  }
  return out;
}

function countBy(items, keyFn) {
  const out = {};
  for (const item of items) {
    const key = keyFn(item);
    out[key] = (out[key] ?? 0) + 1;
  }
  return out;
}

function userTurnKey(u) {
  return `${u.source}\0${u.sessionId}\0${u.userUuid}`;
}

function attributionKey(a) {
  return `${a.sessionId}\0${a.emitTurnIndex}\0${a.toolUseId}`;
}

function sumTokens(blocks) {
  return blocks.reduce((total, block) => total + block.approxTokens, 0);
}

function sum(values) {
  return values.reduce((total, value) => total + value, 0);
}

function median(values) {
  return percentile(values, 0.5);
}

function percentile(values, p) {
  if (values.length === 0) return null;
  const sorted = [...values].sort((a, b) => a - b);
  const index = Math.min(sorted.length - 1, Math.max(0, Math.ceil(sorted.length * p) - 1));
  return sorted[index];
}

function countOver(values, threshold) {
  if (values.length === 0) return { count: 0, ratio: null };
  const count = values.filter((v) => v > threshold).length;
  return { count, ratio: count / values.length };
}

function renderHuman(report) {
  const lines = [];
  lines.push('User-turn tokenizer measurement');
  lines.push(
    `files: ${report.files.parsed}/${report.files.matched} parsed (${formatSourceCounts(report.files.bySource)})`,
  );
  if (report.files.failed > 0) lines.push(`failed files: ${report.files.failed}`);
  lines.push('');
  lines.push('Token counts: heuristic / cl100k');
  lines.push(renderTokenLine('blocks', report.tokens.blocks));
  lines.push(renderTokenLine('user turns', report.tokens.userTurns));
  lines.push(renderTokenLine('text blocks', report.tokens.byBlockKind.text));
  lines.push(renderTokenLine('tool_result blocks', report.tokens.byBlockKind.tool_result));
  if (report.tokens.missingUserTurns > 0 || report.tokens.blockMismatches > 0) {
    lines.push(
      `pairing: ${report.tokens.missingUserTurns} missing user turns, ${report.tokens.blockMismatches} block mismatches`,
    );
  }
  lines.push('');
  lines.push('Dollar attribution');
  lines.push(
    `total: heuristic ${formatUsd(report.attribution.calls.heuristicTotal)}, cl100k ${formatUsd(report.attribution.calls.cl100kTotal)}, delta ${formatUsd(report.attribution.calls.delta)} (${formatPct(report.attribution.calls.relativeDelta)})`,
  );
  lines.push(
    `per call: p95 absolute dollar delta ${formatUsd(report.attribution.calls.p95AbsDollarDelta)}, p95 relative delta ${formatPct(report.attribution.calls.p95AbsRelativeDelta)}, >5% ${formatCountPct(report.attribution.calls.over5Pct)}`,
  );
  lines.push('');
  lines.push('Top tool ranking');
  if (report.attribution.ranking.top.length === 0) {
    lines.push('(no attributed tool calls)');
  } else {
    for (const row of report.attribution.ranking.top) {
      lines.push(
        `${row.tool}: #${row.heuristicRank ?? '-'} ${formatUsd(row.heuristicCost)} -> #${row.cl100kRank ?? '-'} ${formatUsd(row.cl100kCost)} (${formatUsd(row.delta)})`,
      );
    }
  }
  lines.push(`top-10 rank changes: ${report.attribution.ranking.top10RankChanges}`);
  process.stdout.write(`${lines.join('\n')}\n`);
}

function renderTokenLine(label, stats) {
  return [
    `${label}: n=${stats.count}`,
    `tokens ${formatInt(stats.heuristicTokens)} / ${formatInt(stats.cl100kTokens)}`,
    `median ratio ${formatNumber(stats.medianRatio)}`,
    `p95 |error| ${formatPct(stats.p95AbsDeviation)}`,
    `>20% ${formatCountPct(stats.over20Pct)}`,
  ].join(', ');
}

function formatSourceCounts(counts) {
  return Object.entries(counts)
    .sort(([a], [b]) => a.localeCompare(b))
    .map(([source, count]) => `${source} ${count}`)
    .join(', ');
}

function formatInt(value) {
  return Math.round(value).toLocaleString('en-US');
}

function formatNumber(value) {
  return value === null ? 'n/a' : value.toFixed(3);
}

function formatPct(value) {
  return value === null ? 'n/a' : `${(value * 100).toFixed(1)}%`;
}

function formatCountPct(value) {
  return value.ratio === null
    ? `${value.count} (n/a)`
    : `${value.count} (${formatPct(value.ratio)})`;
}

function formatUsd(value) {
  if (value === null) return 'n/a';
  return `$${value.toFixed(6)}`;
}

function printHelp() {
  process.stdout.write(`Usage: pnpm run tokenizer:measure -- [options] <session-file-or-directory...>

Options:
  --source auto|claude|codex|opencode  Source parser to use (default: auto)
  --json                               Emit machine-readable JSON
  -h, --help                           Show this help

The script parses each matched session twice: once with the bytes/4 heuristic
fallback and once with cl100k. It reports block-level token drift plus the
impact on the waste allocator's per-tool-call dollar attribution and ranking.
`);
}

main().catch((err) => {
  process.stderr.write(`${err instanceof Error ? err.message : String(err)}\n`);
  process.exitCode = 1;
});
