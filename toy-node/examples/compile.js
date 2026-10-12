// Compiles Counter.sol with solc-js and writes the creation and runtime bytecode.
// Usage: npm i solc@0.8.24 && node compile.js
const solc = require('solc');
const fs = require('fs');
const input = {
  language: 'Solidity',
  sources: { 'Counter.sol': { content: fs.readFileSync(__dirname + '/Counter.sol', 'utf8') } },
  settings: {
    // Breeja executes under Shanghai rules (see EVM-NOTES.md), so target it.
    evmVersion: 'shanghai',
    optimizer: { enabled: true, runs: 200 },
    outputSelection: { '*': { '*': ['evm.bytecode.object', 'evm.deployedBytecode.object', 'abi'] } },
  },
};
const out = JSON.parse(solc.compile(JSON.stringify(input)));
if (out.errors) for (const e of out.errors) console.error(e.formattedMessage);
const c = out.contracts['Counter.sol'].Counter;
fs.writeFileSync(__dirname + '/Counter.bin', c.evm.bytecode.object);
fs.writeFileSync(__dirname + '/Counter.runtime.bin', c.evm.deployedBytecode.object);
console.log('creation', c.evm.bytecode.object.length / 2, 'bytes; runtime', c.evm.deployedBytecode.object.length / 2, 'bytes');
