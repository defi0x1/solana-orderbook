// Codama tree for the clob program — the single source of truth for the
// generated Rust client (sdk/rust, used by the tests crate) and the JS client
// (sdk/js). Wire formats here must match program/src exactly; the tests crate
// exercises every builder against the real program, so a drift fails CI.
import path from 'node:path';
import { readdir, readFile, writeFile } from 'node:fs/promises';
import { fileURLToPath } from 'node:url';
import { createFromRoot } from 'codama';
import {
  rootNode,
  programNode,
  accountNode,
  definedTypeNode,
  definedTypeLinkNode,
  instructionNode,
  instructionAccountNode,
  instructionArgumentNode,
  structTypeNode,
  structFieldTypeNode,
  arrayTypeNode,
  prefixedCountNode,
  numberTypeNode,
  publicKeyTypeNode,
  bytesTypeNode,
  fixedSizeTypeNode,
  numberValueNode,
  fieldDiscriminatorNode,
  errorNode,
} from 'codama';
import { renderVisitor as renderRustVisitor } from '@codama/renderers-rust';
import { renderVisitor as renderJsVisitor } from '@codama/renderers-js';

const __dirname = path.dirname(fileURLToPath(import.meta.url));
const workspace = path.join(__dirname, '..', '..');
const jsOutput = path.join(workspace, 'sdk', 'js', 'src', 'generated');

const stripTrailingWhitespace = async (directory) => {
  for (const entry of await readdir(directory, { withFileTypes: true })) {
    const entryPath = path.join(directory, entry.name);
    if (entry.isDirectory()) {
      await stripTrailingWhitespace(entryPath);
    } else if (entry.name.endsWith('.ts')) {
      const source = await readFile(entryPath, 'utf8');
      await writeFile(entryPath, source.replace(/[ \t]+$/gm, ''));
    }
  }
};

const u8 = () => numberTypeNode('u8');
const u16 = () => numberTypeNode('u16');
const u32 = () => numberTypeNode('u32');
const u64 = () => numberTypeNode('u64');
const field = (name, type) => structFieldTypeNode({ name, type });
const padding = (name, size) => field(name, fixedSizeTypeNode(bytesTypeNode(), size));

const discriminator = (value) =>
  instructionArgumentNode({
    name: 'discriminator',
    type: u8(),
    defaultValue: numberValueNode(value),
    defaultValueStrategy: 'omitted',
  });
const arg = (name, type) => instructionArgumentNode({ name, type });
const acct = (name, { signer = false, writable = false } = {}) =>
  instructionAccountNode({ name, isSigner: signer, isWritable: writable });

const ix = (name, value, accounts, args) =>
  instructionNode({
    name,
    discriminators: [fieldDiscriminatorNode('discriminator')],
    accounts,
    arguments: [discriminator(value), ...args],
  });

const quoteType = definedTypeNode({
  name: 'quote',
  type: structTypeNode([field('tick', u32()), field('lots', u32()), field('flags', u8())]),
});

const seatType = definedTypeNode({
  name: 'seat',
  type: structTypeNode([
    field('owner', publicKeyTypeNode()),
    field('baseFree', u64()),
    field('quoteFree', u64()),
    field('baseLocked', u64()),
    field('quoteLocked', u64()),
    field('ordersHead', u16()),
    field('orderCount', u16()),
    padding('padding', 12),
  ]),
});

const levelType = definedTypeNode({
  name: 'level',
  type: structTypeNode([
    field('head', u16()),
    field('tail', u16()),
    field('totalLots', u64()),
    padding('padding', 4),
  ]),
});

const orderNodeType = definedTypeNode({
  name: 'orderNode',
  type: structTypeNode([
    field('prev', u16()),
    field('next', u16()),
    field('makerPrev', u16()),
    field('makerNext', u16()),
    field('tick', u32()),
    field('makerAndSide', u16()),
    field('lots', u32()),
    field('expirySlot', u32()),
    field('seq', u64()),
  ]),
});

const configAccount = accountNode({
  name: 'config',
  docs: ['Venue permissions shared by every market that names this config.'],
  data: structTypeNode([
    field('version', u8()),
    field('bump', u8()),
    field('authority', publicKeyTypeNode()),
    field('seatAuthority', publicKeyTypeNode()),
    field('marketAuthority', publicKeyTypeNode()),
    padding('padding', 30),
  ]),
});

const marketAccount = accountNode({
  name: 'market',
  docs: [
    'Fixed 320-byte header of the market account. The bitmaps, levels, seats',
    'and order pool regions follow at fixed offsets (see program constants).',
  ],
  data: structTypeNode([
    field('version', u8()),
    field('bump', u8()),
    field('baseMint', publicKeyTypeNode()),
    field('quoteMint', publicKeyTypeNode()),
    field('baseVault', publicKeyTypeNode()),
    field('quoteVault', publicKeyTypeNode()),
    field('vaultAuthority', publicKeyTypeNode()),
    field('authority', publicKeyTypeNode()),
    field('config', publicKeyTypeNode()),
    field('tickSize', u64()),
    field('baseLotSize', u64()),
    field('anchorTick', u32()),
    field('tickLimit', u32()),
    field('feeBps', u16()),
    field('maxLotsPerOrder', u32()),
    field('orderPoolCapacity', u32()),
    field('freeHead', u16()),
    field('freeCount', u32()),
    field('seq', numberTypeNode('u128')),
    field('orderSeq', u64()),
    field('feesAccruedQuote', u64()),
    field('baseTokenProgram', u8()),
    field('quoteTokenProgram', u8()),
    field('baseDecimals', u8()),
    field('quoteDecimals', u8()),
    padding('padding', 18),
  ]),
});

const quoteVec = () => arrayTypeNode(definedTypeLinkNode('quote'), prefixedCountNode(u8()));

const instructions = [
  // Admin — discriminators from 0
  ix(
    'createMarket',
    0,
    [
      acct('market', { signer: true, writable: true }),
      acct('baseMint'),
      acct('quoteMint'),
      acct('baseVault'),
      acct('quoteVault'),
      acct('authority'),
      acct('config'),
    ],
    [
      arg('tickSize', u64()),
      arg('baseLotSize', u64()),
      arg('anchorTick', u32()),
      arg('feeBps', u16()),
      arg('maxLotsPerOrder', u32()),
    ],
  ),
  ix(
    'updateFee',
    2,
    [acct('authority', { signer: true }), acct('market', { writable: true })],
    [arg('feeBps', u16())],
  ),
  ix(
    'collectFees',
    3,
    [
      acct('authority', { signer: true }),
      acct('market', { writable: true }),
      acct('quoteVault', { writable: true }),
      acct('authorityQuoteAta', { writable: true }),
      acct('vaultAuthority'),
      acct('quoteMint'),
      acct('quoteTokenProgram'),
    ],
    [],
  ),

  ix(
    'createConfig',
    4,
    [
      acct('payer', { signer: true, writable: true }),
      acct('config', { writable: true }),
      acct('authority', { signer: true }),
      acct('systemProgram'),
    ],
    [arg('seatAuthority', publicKeyTypeNode()), arg('marketAuthority', publicKeyTypeNode())],
  ),
  ix(
    'updateConfig',
    5,
    [acct('authority', { signer: true }), acct('config', { writable: true })],
    [arg('seatAuthority', publicKeyTypeNode()), arg('marketAuthority', publicKeyTypeNode())],
  ),

  // Seat — discriminators from 10
  ix(
    'claimSeat',
    10,
    [
      acct('seatAuthority', { signer: true }),
      acct('owner'),
      acct('config'),
      acct('market', { writable: true }),
    ],
    [],
  ),
  ix(
    'closeSeat',
    11,
    [acct('owner', { signer: true }), acct('market', { writable: true })],
    [arg('seatIndex', u16())],
  ),
  ix(
    'deposit',
    12,
    [
      acct('owner', { signer: true }),
      acct('market', { writable: true }),
      acct('ownerBaseAta', { writable: true }),
      acct('ownerQuoteAta', { writable: true }),
      acct('baseVault', { writable: true }),
      acct('quoteVault', { writable: true }),
      acct('baseMint'),
      acct('quoteMint'),
      acct('baseTokenProgram'),
      acct('quoteTokenProgram'),
    ],
    [arg('seatIndex', u16()), arg('baseAtoms', u64()), arg('quoteAtoms', u64())],
  ),
  ix(
    'withdraw',
    13,
    [
      acct('owner', { signer: true }),
      acct('market', { writable: true }),
      acct('ownerBaseAta', { writable: true }),
      acct('ownerQuoteAta', { writable: true }),
      acct('baseVault', { writable: true }),
      acct('quoteVault', { writable: true }),
      acct('vaultAuthority'),
      acct('baseMint'),
      acct('quoteMint'),
      acct('baseTokenProgram'),
      acct('quoteTokenProgram'),
    ],
    [arg('seatIndex', u16()), arg('baseAtoms', u64()), arg('quoteAtoms', u64())],
  ),

  // Maker — discriminators from 20
  ix(
    'massQuote',
    20,
    [acct('owner', { signer: true }), acct('market', { writable: true })],
    [
      arg('seatIndex', u16()),
      arg('expirySlot', u32()),
      arg('bids', quoteVec()),
      arg('asks', quoteVec()),
    ],
  ),
  ix(
    'placeLimit',
    21,
    [acct('owner', { signer: true }), acct('market', { writable: true })],
    [
      arg('seatIndex', u16()),
      arg('side', u8()),
      arg('tick', u32()),
      arg('lots', u32()),
      arg('expirySlot', u32()),
      arg('flags', u8()),
    ],
  ),
  ix(
    'cancel',
    22,
    [acct('owner', { signer: true }), acct('market', { writable: true })],
    [arg('seatIndex', u16()), arg('nodeIndex', u16()), arg('orderSeq', u64())],
  ),

  // Taker — discriminators from 30
  ix(
    'placeTake',
    30,
    [acct('owner', { signer: true }), acct('market', { writable: true })],
    [
      arg('seatIndex', u16()),
      arg('side', u8()),
      arg('limitTick', u32()),
      arg('maxLots', u32()),
      arg('minOut', u64()),
      arg('flags', u8()),
    ],
  ),
  ix(
    'swap',
    31,
    [
      acct('user', { signer: true }),
      acct('market', { writable: true }),
      acct('userBaseAta', { writable: true }),
      acct('userQuoteAta', { writable: true }),
      acct('baseVault', { writable: true }),
      acct('quoteVault', { writable: true }),
      acct('vaultAuthority'),
      acct('baseMint'),
      acct('quoteMint'),
      acct('baseTokenProgram'),
      acct('quoteTokenProgram'),
    ],
    [arg('side', u8()), arg('amountIn', u64()), arg('limitTick', u32()), arg('minOut', u64())],
  ),

  // Hygiene — discriminators from 40
  ix(
    'reanchor',
    40,
    [
      acct('marketAuthority', { signer: true }),
      acct('config'),
      acct('market', { writable: true }),
    ],
    [arg('targetAnchor', u32()), arg('maxLevels', u32())],
  ),
  ix(
    'reapExpired',
    41,
    [acct('market', { writable: true })],
    [arg('side', u8()), arg('startTick', u32()), arg('maxCount', u32())],
  ),
];

const errors = [
  ['notMutable', 'Account expected to be mutable'],
  ['notSigner', 'Account expected to be a signer'],
  ['invalidAccountOwner', 'Account expected to be owned by our program'],
  ['invalidAccountLength', 'The Account data length is not the expected one'],
  ['invalidVersion', 'The Market version is not the expected one'],
  ['invalidSeatIndex', 'The seat index is out of range'],
  ['invalidSeatOwner', 'The seat is not owned by the signer'],
  ['seatTableFull', 'The seat table is full'],
  ['seatNotEmpty', 'The seat still has balances or resting orders'],
  ['tickOutOfWindow', 'The tick is outside the current book window'],
  ['invalidLots', 'The lots amount is zero or above the per-order maximum'],
  ['poolFull', 'The order pool is full'],
  ['orderNotFound', 'The order was not found (already filled, cancelled, or reused slot)'],
  ['wouldCross', 'The order would cross the opposing side'],
  ['payloadNotSorted', 'The mass quote payload is not sorted ascending by tick per side'],
  ['tooManyOrders', 'The seat has reached the maximum number of resting orders'],
  ['slippageExceeded', 'Slippage tolerance exceeded - output amount below minimum threshold'],
  ['selfTrade', 'The order would self trade and the policy is abort'],
  ['invalidFee', 'The Fee is greater than the maximum allowed'],
  ['invalidMarketParams', 'The market parameters are invalid or would overflow u64 math'],
  ['insufficientBalance', 'The seat balance is insufficient'],
  ['invalidTokenAddress', 'The token account address is not the expected one'],
  ['invalidProgramAuthority', 'The Program authority is not the expected one'],
  ['invalidState', 'The instruction is not valid on this account state'],
  ['zeroAuthority', 'A delegated authority pubkey may not be the zero address'],
  ['unsupportedTokenProgram', "A mint's owner is neither the legacy Token program nor Token-2022"],
  [
    'permanentDelegateNotAllowed',
    'A mint carries the Token-2022 PermanentDelegate extension, which is not allowed',
  ],
].map(([name, message], code) => errorNode({ name, code, message }));

const program = (instructionNodes) =>
  programNode({
    name: 'clob',
    publicKey: 'BzcBf4JYcpWhFuw1smYiBSt647PJraT1Wxm2soBTSsvb',
    version: '0.0.1',
    accounts: [configAccount, marketAccount],
    definedTypes: [quoteType, seatType, levelType, orderNodeType],
    instructions: instructionNodes,
    errors,
  });

// The Rust facade provides MassQuote itself because Codama's U8PrefixVec has
// no public constructor. JavaScript arrays do not have that limitation.
const rustCodama = createFromRoot(
  rootNode(program(instructions.filter(({ name }) => name !== 'massQuote'))),
);
const jsCodama = createFromRoot(rootNode(program(instructions)));

await rustCodama.accept(
  renderRustVisitor(path.join(workspace, 'sdk', 'rust', 'src', 'generated'), {
    formatCode: true,
    crateFolder: path.join(workspace, 'sdk', 'rust'),
    anchorTraits: false,
  }),
);

await jsCodama.accept(
  renderJsVisitor(jsOutput, {
    formatCode: false,
  }),
);
await stripTrailingWhitespace(jsOutput);

console.log('generated: sdk/rust/src/generated + sdk/js/src/generated');
