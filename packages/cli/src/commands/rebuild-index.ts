import { rebuildIndex } from '@relayburn/ledger';

export async function runRebuildIndex(): Promise<number> {
  const { ids, content } = await rebuildIndex();
  process.stdout.write(
    `rebuilt ledger index: ${ids} id hashes, ${content} content fingerprints\n`,
  );
  return 0;
}
