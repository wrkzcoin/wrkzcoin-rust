// Copyright (c) 2026, The WrkzCoin developers
//
// Please see the included LICENSE file for more information

// Generates block-level conformance vectors from the reference implementation:
// the genesis block as the Currency object builds it, and one synthetic block
// per major version so the hashing blob construction can be checked end to end.
#include <cryptonotecore/CachedBlock.h>
#include <cryptonotecore/CachedTransaction.h>
#include <cryptonotecore/Currency.h>
#include <common/CryptoNoteTools.h>
#include <common/StringTools.h>
#include <common/TransactionExtra.h>
#include <logging/LoggerManager.h>
#include <serialization/SerializationTools.h>

#include <cstdio>
#include <cstring>
#include <memory>

using namespace CryptoNote;

static void dumpBlock(const char *label, const BlockTemplate &b)
{
    CachedBlock cb(b);
    printf("### %s (major %d)\n", label, b.majorVersion);
    printf("block blob (BlockTemplate serialization) = %s\n", Common::toHex(toBinaryArray(b)).c_str());
    printf("coinbase tx blob = %s\n", Common::toHex(toBinaryArray(b.baseTransaction)).c_str());
    printf("coinbase tx hash = %s\n", Common::podToHex(getObjectHash(b.baseTransaction)).c_str());
    printf("tx tree hash = %s\n", Common::podToHex(cb.getTransactionTreeHash()).c_str());
    printf("header hashing blob (header||treehash||varint(txcount)) = %s\n",
           Common::toHex(cb.getBlockHashingBinaryArray()).c_str());
    if (b.majorVersion >= BLOCK_MAJOR_VERSION_2)
    {
        printf("aux block header hash = keccak(header hashing blob) = %s\n",
               Common::podToHex(cb.getAuxiliaryBlockHeaderHash()).c_str());
        printf("parent block blob (in block, no merkle) = %s\n",
               Common::toHex(cb.getParentBlockBinaryArray(false)).c_str());
        printf("parent block hashing blob full (for block id) = %s\n",
               Common::toHex(cb.getParentBlockHashingBinaryArray(false)).c_str());
        printf("parent block hashing blob header-only (PoW input) = %s\n",
               Common::toHex(cb.getParentBlockHashingBinaryArray(true)).c_str());
    }
    printf("block id = %s\n", Common::podToHex(cb.getBlockHash()).c_str());
    printf("pow hash = %s\n", Common::podToHex(cb.getBlockLongHash()).c_str());
    printf("\n");
}

int main()
{
    auto log = std::make_shared<Logging::LoggerManager>();
    Currency currency = CurrencyBuilder(log).currency();

    printf("## genesis (from Currency)\n");
    const BlockTemplate &g = currency.genesisBlock();
    printf("genesis major=%d minor=%d timestamp=%llu nonce=%u prev=%s\n",
           g.majorVersion, g.minorVersion, (unsigned long long)g.timestamp, g.nonce,
           Common::podToHex(g.previousBlockHash).c_str());
    dumpBlock("genesis", g);

    /* A synthetic block on top of genesis. The coinbase is deterministic: tx
       version 1, unlock 41, one base input at height 1, one output of 1 atomic
       unit to a fixed key, extra = pubkey tag + fixed key. Only the values a
       hashing implementation reads matter here; nothing is validated. */
    printf("## synthetic blocks at height 1, one per major version\n");
    for (uint8_t major = 1; major <= 7; major++)
    {
        BlockTemplate b {};
        b.majorVersion = major;
        b.minorVersion = 0;
        b.timestamp = 1529831318;
        b.nonce = 0x11223344;
        b.previousBlockHash = currency.genesisBlockHash();

        Transaction &tx = b.baseTransaction;
        tx.version = 1;
        tx.unlockTime = 41;
        BaseInput in;
        in.blockIndex = 1;
        tx.inputs.push_back(in);
        TransactionOutput out;
        out.amount = 1;
        KeyOutput ko;
        memset(ko.key.data, 0x33, 32);
        out.target = ko;
        tx.outputs.push_back(out);
        Crypto::PublicKey txPub;
        memset(txPub.data, 0x44, 32);
        addTransactionPublicKeyToExtra(tx.extra, txPub);

        /* one fake transaction hash so the tree hash has two leaves */
        Crypto::Hash fake;
        memset(fake.data, 0x55, 32);
        b.transactionHashes.push_back(fake);

        if (major >= BLOCK_MAJOR_VERSION_2)
        {
            b.parentBlock.majorVersion = BLOCK_MAJOR_VERSION_1;
            b.parentBlock.minorVersion = 0;
            b.parentBlock.transactionCount = 1;
            b.parentBlock.previousBlockHash = Crypto::Hash {};
            b.parentBlock.baseTransaction.version = 1;
            b.parentBlock.baseTransaction.unlockTime = 0;
            BaseInput pin;
            pin.blockIndex = 0;
            b.parentBlock.baseTransaction.inputs.push_back(pin);

            /* what a miner does: mm tag depth 0, merkle root = aux header hash */
            CachedBlock probe(b);
            TransactionExtraMergeMiningTag mm;
            mm.depth = 0;
            mm.merkleRoot = probe.getAuxiliaryBlockHeaderHash();
            b.parentBlock.baseTransaction.extra.clear();
            appendMergeMiningTagToExtra(b.parentBlock.baseTransaction.extra, mm);
        }

        char label[32];
        snprintf(label, sizeof(label), "synthetic v%d", major);
        dumpBlock(label, b);
        printf("checkProofOfWork(difficulty=1) = %d\n\n", currency.checkProofOfWork(CachedBlock(b), 1));
    }

    /* A transaction with one key input and two outputs, to pin the prefix
       serialization and the tx hash / prefix hash relation. Signatures are
       zero-filled (invalid, but the encoding is what is being checked). */
    printf("## synthetic transaction\n");
    {
        Transaction t;
        t.version = 1;
        t.unlockTime = 0;
        KeyInput ki;
        ki.amount = 500;
        ki.outputIndexes = {7, 3, 300};
        memset(ki.keyImage.data, 0x66, 32);
        t.inputs.push_back(ki);
        for (uint64_t amount : {400ull, 90ull})
        {
            TransactionOutput o;
            o.amount = amount;
            KeyOutput k;
            memset(k.key.data, (int)(0x70 + amount % 7), 32);
            o.target = k;
            t.outputs.push_back(o);
        }
        Crypto::PublicKey txPub;
        memset(txPub.data, 0x77, 32);
        addTransactionPublicKeyToExtra(t.extra, txPub);
        std::vector<uint8_t> nonce = {0x00};
        nonce.resize(33, 0x88); /* long payment id 0x88.. */
        addExtraNonceToTransactionExtra(t.extra, nonce);
        t.signatures.resize(1);
        t.signatures[0].resize(3);

        CachedTransaction ct(t);
        printf("prefix blob = %s\n", Common::toHex(toBinaryArray(static_cast<TransactionPrefix>(t))).c_str());
        printf("full blob (with 3 zero signatures) = %s\n", Common::toHex(ct.getTransactionBinaryArray()).c_str());
        printf("prefix hash = %s\n", Common::podToHex(ct.getTransactionPrefixHash()).c_str());
        printf("tx hash = keccak(full blob) = %s\n", Common::podToHex(ct.getTransactionHash()).c_str());
        printf("fee = %llu\n", (unsigned long long)ct.getTransactionFee());
    }

    printf("## done\n");
    return 0;
}
