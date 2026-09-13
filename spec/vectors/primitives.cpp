// Copyright (c) 2026, The WrkzCoin developers
//
// Please see the included LICENSE file for more information

// Generates conformance vectors from the reference C/C++ implementation.
// Build (MSVC x64, from the repo root):
//   cl /nologo /O2 /EHsc /std:c++17 /I include /I src /I src\platform\msc
//      /I external\argon2\include /I external\nlohmann-json /Fe:vectors.exe vectors.cpp
//      /link build\src\Release\Crypto.lib build\src\Release\Common.lib
//            build\src\Release\Mnemonics.lib <argon2.lib> advapi32.lib
#include <crypto/crypto.h>
#include <crypto/hash.h>
#include <crypto/chacha8.h>
#include <crypto/WalletCrypto.h>
#include <common/Base58.h>
#include <common/Varint.h>
#include <common/CheckDifficulty.h>
#include <mnemonics/Mnemonics.h>

#include <cstdio>
#include <cstring>
#include <string>
#include <vector>

static std::string hex(const void *p, size_t n)
{
    static const char *d = "0123456789abcdef";
    std::string s;
    const uint8_t *b = (const uint8_t *)p;
    for (size_t i = 0; i < n; i++)
    {
        s += d[b[i] >> 4];
        s += d[b[i] & 15];
    }
    return s;
}

static std::vector<uint8_t> fromHex(const std::string &h)
{
    std::vector<uint8_t> out;
    for (size_t i = 0; i + 1 < h.size(); i += 2)
    {
        out.push_back((uint8_t)strtoul(h.substr(i, 2).c_str(), nullptr, 16));
    }
    return out;
}

template<class T> static std::string H(const T &t)
{
    return hex(&t, sizeof(t));
}

int main()
{
    /* ---------------------------------------------------------------- */
    printf("## inputs\n");
    std::vector<std::pair<std::string, std::vector<uint8_t>>> inputs;
    inputs.push_back({"I0 empty", {}});
    inputs.push_back({"I1 'abc'", {'a', 'b', 'c'}});
    {
        std::string s = "This is a test This is a test This is a test";
        inputs.push_back({"I2 44-byte ascii", std::vector<uint8_t>(s.begin(), s.end())});
    }
    {
        std::vector<uint8_t> v;
        for (int i = 0; i < 76; i++) v.push_back((uint8_t)i);
        inputs.push_back({"I3 bytes 0x00..0x4b (76 bytes)", v});
    }
    {
        std::vector<uint8_t> v;
        for (int i = 0; i < 200; i++) v.push_back((uint8_t)i);
        inputs.push_back({"I4 bytes 0x00..0xc7 (200 bytes)", v});
    }
    for (auto &in : inputs)
    {
        printf("%s = %s\n", in.first.c_str(), hex(in.second.data(), in.second.size()).c_str());
    }

    /* ---------------------------------------------------------------- */
    printf("\n## cn_fast_hash (keccak-256, 0x01 padding)\n");
    for (auto &in : inputs)
    {
        Crypto::Hash h;
        Crypto::cn_fast_hash(in.second.data(), in.second.size(), h);
        printf("%s -> %s\n", in.first.c_str(), H(h).c_str());
    }

    /* ---------------------------------------------------------------- */
    printf("\n## cn_slow_hash_v0 (block major v1..v3) [light=0 variant=0 2MiB scratch 2^20 iter mask 0x1FFFF0]\n");
    for (auto &in : inputs)
    {
        Crypto::Hash h;
        Crypto::cn_slow_hash_v0(in.second.data(), in.second.size(), h);
        printf("%s -> %s\n", in.first.c_str(), H(h).c_str());
    }

    printf("\n## cn_lite_slow_hash_v1 (block major v4) [light=1 variant=1 1MiB scratch 2^19 iter mask 0xFFFF0] (input >= 43 bytes only)\n");
    for (auto &in : inputs)
    {
        if (in.second.size() < 43) continue;
        Crypto::Hash h;
        Crypto::cn_lite_slow_hash_v1(in.second.data(), in.second.size(), h);
        printf("%s -> %s\n", in.first.c_str(), H(h).c_str());
    }

    printf("\n## cn_turtle_lite_slow_hash_v2 (block major v5) [light=1 variant=2 256KiB scratch 2^17 iter mask 0x1FFF0]\n");
    for (auto &in : inputs)
    {
        Crypto::Hash h;
        Crypto::cn_turtle_lite_slow_hash_v2(in.second.data(), in.second.size(), h);
        printf("%s -> %s\n", in.first.c_str(), H(h).c_str());
    }

    printf("\n## chukwa_slow_hash (block major v6) [argon2id t=4 m=256KiB p=1 salt=first 16 bytes of input, 32-byte tag] (input >= 16 bytes only)\n");
    for (auto &in : inputs)
    {
        if (in.second.size() < 16) continue;
        Crypto::Hash h;
        Crypto::chukwa_slow_hash(in.second.data(), in.second.size(), h);
        printf("%s -> %s\n", in.first.c_str(), H(h).c_str());
    }

    printf("\n## cn_upx (block major v7, tx PoW) [light=2 variant=2 128KiB scratch 2^15 iter mask 0x1FFF0]\n");
    for (auto &in : inputs)
    {
        Crypto::Hash h;
        Crypto::cn_upx(in.second.data(), in.second.size(), h);
        printf("%s -> %s\n", in.first.c_str(), H(h).c_str());
    }

    /* ---------------------------------------------------------------- */
    printf("\n## tree_hash (leaves: leaf[i] = cn_fast_hash(single byte i))\n");
    {
        std::vector<Crypto::Hash> leaves;
        for (int i = 0; i < 9; i++)
        {
            uint8_t b = (uint8_t)i;
            Crypto::Hash h;
            Crypto::cn_fast_hash(&b, 1, h);
            leaves.push_back(h);
            printf("leaf[%d] = %s\n", i, H(h).c_str());
        }
        for (size_t n : {1, 2, 3, 4, 5, 7, 8, 9})
        {
            Crypto::Hash root;
            Crypto::tree_hash(leaves.data(), n, root);
            printf("tree_hash(count=%zu) = %s\n", n, H(root).c_str());
        }
    }

    /* ---------------------------------------------------------------- */
    printf("\n## keys\n");
    Crypto::SecretKey seed;
    memset(seed.data, 0x11, 32);
    Crypto::PublicKey spendPub;
    Crypto::SecretKey spendSec;
    Crypto::generate_deterministic_keys(spendPub, spendSec, seed);
    printf("seed (32 x 0x11) = %s\n", H(seed).c_str());
    printf("spend_secret = sc_reduce32(seed) = %s\n", H(spendSec).c_str());
    printf("spend_public = %s\n", H(spendPub).c_str());

    Crypto::SecretKey viewSec;
    Crypto::PublicKey viewPub;
    Crypto::crypto_ops::generateViewFromSpend(spendSec, viewSec, viewPub);
    {
        Crypto::Hash k;
        Crypto::cn_fast_hash(spendSec.data, 32, k);
        printf("keccak(spend_secret) = %s\n", H(k).c_str());
    }
    printf("view_secret = sc_reduce32(keccak(spend_secret)) = %s\n", H(viewSec).c_str());
    printf("view_public = %s\n", H(viewPub).c_str());

    {
        std::string keys = std::string((const char *)spendPub.data, 32) + std::string((const char *)viewPub.data, 32);
        std::string addr = Tools::Base58::encode_addr(999730, keys);
        printf("address (prefix 999730) = %s (len %zu)\n", addr.c_str(), addr.size());
        std::string pre = Tools::get_varint_data((uint64_t)999730);
        printf("varint(999730) = %s\n", hex(pre.data(), pre.size()).c_str());
        Crypto::Hash chk;
        std::string buf = pre + keys;
        Crypto::cn_fast_hash(buf.data(), buf.size(), chk);
        printf("address checksum = first 4 bytes of keccak(varint(prefix)||spend||view) = %s\n", hex(chk.data, 4).c_str());
        uint64_t tag;
        std::string data;
        bool ok = Tools::Base58::decode_addr(addr, tag, data);
        printf("decode_addr ok=%d prefix=%llu payload=%s\n", ok, (unsigned long long)tag, hex(data.data(), data.size()).c_str());

        /* integrated address with an 8 byte payment id */
        std::string pid8 = std::string("\x01\x02\x03\x04\x05\x06\x07\x08", 8);
        std::string iaddr = Tools::Base58::encode_addr(999730, pid8 + keys);
        printf("integrated address (pid 0102030405060708) = %s (len %zu)\n", iaddr.c_str(), iaddr.size());
    }

    printf("\n## mnemonic (25 words, electrum-style, crc32 checksum word)\n");
    {
        std::string words = Mnemonics::PrivateKeyToMnemonic(spendSec);
        printf("mnemonic(spend_secret) = %s\n", words.c_str());
        auto [err, back] = Mnemonics::MnemonicToPrivateKey(words);
        printf("round trip error=%d key=%s\n", (int)err.getErrorCode(), H(back).c_str());
    }

    printf("\n## deterministic subwallets (generate_deterministic_subwallet_keys)\n");
    for (uint64_t idx : {1ull, 2ull, 5ull})
    {
        auto [sub, subPub] = Crypto::generate_deterministic_subwallet_keys(spendSec, idx);
        printf("index %llu: secret=%s public=%s\n", (unsigned long long)idx, H(sub).c_str(), H(subPub).c_str());
    }

    /* ---------------------------------------------------------------- */
    printf("\n## output derivation (sender tx key -> receiver)\n");
    Crypto::SecretKey txSeed;
    memset(txSeed.data, 0x22, 32);
    Crypto::PublicKey txPub;
    Crypto::SecretKey txSec;
    Crypto::generate_deterministic_keys(txPub, txSec, txSeed);
    printf("tx_secret = sc_reduce32(32 x 0x22) = %s\n", H(txSec).c_str());
    printf("tx_public = %s\n", H(txPub).c_str());

    Crypto::KeyDerivation derivation;
    Crypto::generate_key_derivation(viewPub, txSec, derivation);
    printf("derivation = 8 * tx_secret * view_public = %s\n", H(derivation).c_str());
    Crypto::KeyDerivation derivation2;
    Crypto::generate_key_derivation(txPub, viewSec, derivation2);
    printf("derivation (receiver side, 8 * view_secret * tx_public) = %s\n", H(derivation2).c_str());

    for (size_t idx : {(size_t)0, (size_t)1, (size_t)300})
    {
        Crypto::EllipticCurveScalar sc;
        Crypto::derivation_to_scalar(derivation, idx, sc);
        Crypto::PublicKey P;
        Crypto::derive_public_key(derivation, idx, spendPub, P);
        Crypto::SecretKey x;
        Crypto::derive_secret_key(derivation, idx, spendSec, x);
        Crypto::PublicKey check;
        Crypto::secret_key_to_public_key(x, check);
        Crypto::KeyImage ki;
        Crypto::generate_key_image(P, x, ki);
        Crypto::PublicKey underived;
        Crypto::underive_public_key(derivation, idx, P, underived);
        printf("output index %zu:\n", idx);
        printf("  derivation_to_scalar = %s\n", H(sc).c_str());
        printf("  one_time_public (P = Hs(D||varint(i))*G + spend_public) = %s\n", H(P).c_str());
        printf("  one_time_secret (x = Hs(D||varint(i)) + spend_secret) = %s\n", H(x).c_str());
        printf("  x*G == P : %d\n", check == P);
        printf("  key_image (x * Hp(P)) = %s\n", H(ki).c_str());
        printf("  underive -> spend_public : %d\n", underived == spendPub);
    }

    printf("\n## hash_to_scalar / hash_to_ec\n");
    {
        Crypto::EllipticCurveScalar s;
        Crypto::hashToScalar("abc", 3, s);
        printf("hash_to_scalar('abc') = sc_reduce32(keccak('abc')) = %s\n", H(s).c_str());
        Crypto::PublicKey p;
        Crypto::hash_data_to_ec((const uint8_t *)"abc", 3, p);
        printf("hash_to_ec('abc') = 8 * fromfe(keccak('abc')) = %s\n", H(p).c_str());
        Crypto::PublicKey hp;
        Crypto::hash_data_to_ec(spendPub.data, 32, hp);
        printf("Hp(spend_public) = %s\n", H(hp).c_str());
    }

    /* ---------------------------------------------------------------- */
    printf("\n## encrypted short payment id\n");
    {
        uint8_t buf[33];
        memcpy(buf, &derivation, 32);
        buf[32] = 0x8d;
        Crypto::Hash ks;
        Crypto::cn_fast_hash(buf, 33, ks);
        printf("keystream = keccak(derivation || 0x8d) = %s\n", H(ks).c_str());
        uint8_t pid[8] = {1, 2, 3, 4, 5, 6, 7, 8};
        for (int i = 0; i < 8; i++) pid[i] ^= ks.data[i];
        printf("encrypt(0102030405060708) = %s\n", hex(pid, 8).c_str());
    }

    /* ---------------------------------------------------------------- */
    printf("\n## varint\n");
    for (uint64_t v : {0ull, 1ull, 127ull, 128ull, 300ull, 999730ull, 4294967296ull, 18446744073709551615ull})
    {
        std::string s = Tools::get_varint_data(v);
        printf("varint(%llu) = %s\n", (unsigned long long)v, hex(s.data(), s.size()).c_str());
    }

    printf("\n## base58 (CryptoNote block variant, 8-byte blocks -> 11 chars)\n");
    for (const char *s : {"", "a", "hello", "hello world!", "\x00\x00\x00\x01"})
    {
        std::string in(s, s[0] == 0 ? 4 : strlen(s));
        printf("encode(%s) = %s\n", hex(in.data(), in.size()).c_str(), Tools::Base58::encode(in).c_str());
    }

    /* ---------------------------------------------------------------- */
    printf("\n## check_hash(hash, difficulty)\n");
    {
        Crypto::Hash h;
        Crypto::cn_fast_hash("abc", 3, h);
        for (uint64_t d : {1ull, 1000ull, 1ull << 20, 1ull << 40, 1ull << 60})
        {
            printf("check_hash(keccak('abc'), %llu) = %d\n", (unsigned long long)d, CryptoNote::check_hash(h, d));
        }
        Crypto::Hash z;
        memset(z.data, 0, 32);
        z.data[0] = 1;
        printf("check_hash(0100..00, 2^63) = %d\n", CryptoNote::check_hash(z, 1ull << 63));
        memset(z.data, 0xff, 32);
        printf("check_hash(ff..ff, 1) = %d\n", CryptoNote::check_hash(z, 1));
        printf("check_hash(ff..ff, 2) = %d\n", CryptoNote::check_hash(z, 2));
    }

    /* ---------------------------------------------------------------- */
    printf("\n## genesis block\n");
    {
        const char *GENESIS =
            "012801ff00038090cad2c60e02484ab563a5ec4cb8aa159b878e4ca0a417e7258ec4fd338128059f2b7"
            "193dcaa8090cad2c60e02655ed6ab140ef3ca45d8d913125b8bc8917c590af4d1b9d7b4a67396e4a764"
            "088090cad2c60e020e06bf1587f9768cfd735a95e8254e98c68604f690e699f8403058422ede0428210"
            "1c47eee4cfef6f30b5368d0251ad66a5800e2f0b2b70a4a3034c7bba3c5d0d6e0";
        auto tx = fromHex(GENESIS);
        Crypto::Hash txh;
        Crypto::cn_fast_hash(tx.data(), tx.size(), txh);
        printf("genesis coinbase tx hash = %s\n", H(txh).c_str());
        /* hashing blob: major(1) minor(0) timestamp(0) prev(32x00) nonce(70 LE u32) || tree_hash([txh]) || varint(1) */
        std::vector<uint8_t> blob = {0x01, 0x00, 0x00};
        blob.insert(blob.end(), 32, 0);
        blob.push_back(70);
        blob.push_back(0);
        blob.push_back(0);
        blob.push_back(0);
        Crypto::Hash root;
        Crypto::tree_hash(&txh, 1, root);
        blob.insert(blob.end(), root.data, root.data + 32);
        blob.push_back(1);
        printf("genesis hashing blob = %s\n", hex(blob.data(), blob.size()).c_str());
        Crypto::Hash id;
        Crypto::cn_fast_hash(blob.data(), blob.size(), id);
        printf("keccak(blob) WITHOUT the varint length prefix (NOT the block id; the block id is keccak(varint(72) || blob) = 877e55b4e902b9bf4c9e0a7c16440f449339d56679c49d62261ae5c92596a6ce, see blocks.txt) = %s\n", H(id).c_str());
        Crypto::Hash pow;
        Crypto::cn_slow_hash_v0(blob.data(), blob.size(), pow);
        printf("genesis pow hash = cn_slow_hash_v0(blob) = %s\n", H(pow).c_str());
    }

    /* ---------------------------------------------------------------- */
    printf("\n## chacha8 (legacy WalletGreen container)\n");
    {
        uint8_t key[32], iv[8];
        for (int i = 0; i < 32; i++) key[i] = (uint8_t)i;
        for (int i = 0; i < 8; i++) iv[i] = (uint8_t)i;
        const char *pt = "hello wallet";
        char ct[64];
        Crypto::chacha8(pt, strlen(pt), key, iv, ct);
        printf("chacha8(key=00..1f, iv=00..07, 'hello wallet') = %s\n", hex(ct, strlen(pt)).c_str());
        Crypto::chacha8_key pk;
        Crypto::generate_chacha8_key("password", pk);
        printf("generate_chacha8_key('password') = cn_slow_hash_v0('password')[0..32] = %s\n", H(pk).c_str());
    }

    /* ---------------------------------------------------------------- */
    printf("\n## wallet file crypto (PBKDF2-HMAC-SHA256 + AES-128-CBC PKCS7, iv = salt)\n");
    {
        uint8_t salt[16];
        for (int i = 0; i < 16; i++) salt[i] = (uint8_t)i;
        auto key = WalletCrypto::deriveKey("password", salt, 16, 500000, 16);
        printf("deriveKey('password', salt=00..0f, 500000 iters, 16) = %s\n", hex(key.data(), key.size()).c_str());
        auto key2 = WalletCrypto::deriveKey("password", salt, 16, 10000, 16);
        printf("deriveKey('password', salt=00..0f, 10000 iters, 16) = %s\n", hex(key2.data(), key2.size()).c_str());
        std::string ct = WalletCrypto::encrypt("hello wallet", key.data(), salt);
        printf("encrypt('hello wallet', key, iv=salt) = %s\n", hex(ct.data(), ct.size()).c_str());
        auto pt = WalletCrypto::decrypt(ct, key.data(), salt);
        printf("decrypt ok=%d -> '%s'\n", pt.has_value(), pt ? pt->c_str() : "");
    }

    printf("\n## done\n");
    return 0;
}
