import { mkdirSync, mkdtempSync, rmSync } from 'node:fs';
import { join, resolve } from 'node:path';
import { spawnSync } from 'node:child_process';
import { randomBytes } from 'node:crypto';
import { fileURLToPath } from 'node:url';

const approved = '/tmp/opencode';
const fixedSeeds = ['11', '29', '73'];
const unsignedSeed = /^(?:0|[1-9]\d{0,19})$/;
const maxU64 = 18_446_744_073_709_551_615n;

export function validateSeed(value, source = 'seed') {
  if (!unsignedSeed.test(value) || BigInt(value) > maxU64) {
    throw new Error(`${source} must be an unsigned 64-bit decimal integer`);
  }
  return value;
}

export function requestedSeed(argv, environment) {
  const cli = argv.filter((argument) => argument.startsWith('--seed='));
  const index = argv.indexOf('--seed');
  if (index !== -1) {
    if (index + 1 >= argv.length || argv[index + 1].startsWith('--') || cli.length) {
      throw new Error('Use exactly one --seed <unsigned-64-bit-decimal> override');
    }
    return { seed: validateSeed(argv[index + 1], '--seed'), source: 'cli' };
  }
  if (cli.length > 1) throw new Error('Use exactly one --seed override');
  if (cli.length === 1) return { seed: validateSeed(cli[0].slice('--seed='.length), '--seed'), source: 'cli' };
  if (environment.AMTE_RELIABILITY_RANDOM_SEED !== undefined) {
    return { seed: validateSeed(environment.AMTE_RELIABILITY_RANDOM_SEED, 'AMTE_RELIABILITY_RANDOM_SEED'), source: 'env' };
  }
  return { seed: randomBytes(8).readBigUInt64BE().toString(), source: 'system-random' };
}

export function campaignSeeds(argv, environment) {
  const random = requestedSeed(argv, environment);
  return { seeds: [...fixedSeeds, random.seed], random };
}

export function main(argv = process.argv.slice(2), environment = process.env) {
  const full = argv.includes('--full');
  let campaign;
  try {
    campaign = campaignSeeds(argv, environment);
  } catch (error) {
    console.error(error.message);
    return 64;
  }
  const requested = environment.AMTE_RELIABILITY_ROOT;
  if (requested && !resolve(requested).startsWith(`${approved}/`)) {
    console.error(`Refusing destructive reliability fixtures outside ${approved}: ${requested}`);
    return 64;
  }
  mkdirSync(approved, { recursive: true, mode: 0o700 });
  const root = mkdtempSync(join(approved, 'amte-reliability-'));
  const env = {
    ...environment,
    AMTE_RELIABILITY_ROOT: root,
    AMTE_RELIABILITY_SEEDS: campaign.seeds.join(','),
  };
  // Print before the first subprocess so a killed gate can always be replayed.
  console.log(`AMTE_RELIABILITY_SEED random=${campaign.random.seed} source=${campaign.random.source} all=${campaign.seeds.join(',')}`);
  const testList = spawnSync('cargo', ['test', '--manifest-path', 'src-tauri/Cargo.toml', '--', '--list'], { encoding: 'utf8', env });
  if (testList.status !== 0) {
    process.stderr.write(testList.stderr || 'Could not enumerate reliability tests.\n');
    rmSync(root, { recursive: true, force: true });
    return testList.status ?? 1;
  }
  const saveOutcomeTests = (testList.stdout.match(/^tests::save_outcome_.*: test$/gm) || []).length;
  const stagedFileFaultTests = (testList.stdout.match(/^tests::staged_save_faults_.*: test$/gm) || []).length;
  const commands = [
    ['npm', ['test'], 120_000],
    ['cargo', ['test', '--manifest-path', 'src-tauri/Cargo.toml', '--', '--nocapture'], 360_000],
  ];
  if (full) commands.push(
    ['npm', ['run', 'lint'], 120_000],
    ['npm', ['run', 'build'], 120_000],
    ['npm', ['run', 'benchmark'], 180_000],
    ['npm', ['run', 'search:browser'], 180_000],
    ['cargo', ['fmt', '--manifest-path', 'src-tauri/Cargo.toml', '--check'], 120_000],
    ['cargo', ['check', '--manifest-path', 'src-tauri/Cargo.toml'], 240_000],
    ['cargo', ['clippy', '--manifest-path', 'src-tauri/Cargo.toml', '--', '-D', 'warnings'], 360_000],
    ['cargo', ['build', '--manifest-path', 'src-tauri/Cargo.toml', '--release'], 600_000],
    ['git', ['diff', '--check'], 120_000],
  );
  let failures = 0;
  const executions = [];
  let removed = false;
  try {
    for (const [command, args, timeout] of commands) {
      const result = spawnSync(command, args, { stdio: 'inherit', env, timeout, killSignal: 'SIGTERM' });
      if (result.status !== 0) failures += 1;
      if (result.error) console.error(`${[command, ...args].join(' ')} exceeded its ${timeout / 1_000}s bound: ${result.error.message}`);
      executions.push({ command: [command, ...args].join(' '), status: result.status ?? 1, timeoutMs: timeout, timedOut: result.error?.code === 'ETIMEDOUT' });
    }
  } finally {
    rmSync(root, { recursive: true, force: true });
    removed = true;
  }
  console.log(JSON.stringify({
    campaign: full ? 'full' : 'bounded-default',
    fixedSeeds,
    randomSeed: campaign.random.seed,
    randomSeedSource: campaign.random.source,
    seeds: campaign.seeds,
    faultMatrix: { discoveredSaveOutcomeTests: saveOutcomeTests, discoveredStagedFileFaultTests: stagedFileFaultTests },
    executedCommands: executions.length,
    successfulCommands: executions.filter(({ status }) => status === 0).length,
    executions,
    temporaryRootRemoved: removed,
    failures,
  }));
  return failures ? 1 : 0;
}

if (process.argv[1] && resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  process.exitCode = main();
}
