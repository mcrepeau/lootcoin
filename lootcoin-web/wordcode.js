/**
 * wordcode.js — deterministic human-readable address alias.
 *
 * Encodes the first 4 bytes of a hex address as:
 *   byte 0  →  one of 256 adjectives  (8 bits)
 *   byte 1  →  one of 256 animals     (8 bits)
 *   bytes 2-3 → 5-digit number        (16 bits, 00000–65535)
 *
 * Examples: "silent-Walrus-07421", "frozen-Eagle-31802"
 * ~4.3 billion unique combinations. Pure display alias — the full hex
 * address remains the canonical identifier used in all transactions.
 */

const ADJECTIVES = [
  // 0x00–0x0F
  "abstract","active","agile","amber","ancient","angular","arctic","argent",
  "arid","atomic","autumn","azure","bare","binary","blazing","bleak",
  // 0x10–0x1F
  "blind","bold","brave","brief","bright","broken","calm","carven",
  "cerulean","chrome","cipher","cloaked","cobalt","cold","cool","cosmic",
  // 0x20–0x2F
  "covert","crimson","crystal","cyan","cyclic","dark","dawning","dead",
  "deep","dense","digital","dim","distant","divine","dormant","drifting",
  // 0x30–0x3F
  "dusky","dusty","ebony","elder","electric","empty","endless","errant",
  "eternal","exact","fading","fallen","fast","feral","fierce","final",
  // 0x40–0x4F
  "fixed","flat","floating","fluid","foggy","forged","fractal","free",
  "fresh","frozen","giant","gilded","glacial","glowing","golden","grand",
  // 0x50–0x5F
  "great","green","grey","grim","guarded","hallowed","harsh","hashed",
  "hidden","high","hollow","holy","humble","icy","idle","immense",
  // 0x60–0x6F
  "indexed","infinite","inert","inner","intact","iron","jade","jagged",
  "keen","kinetic","large","last","late","latent","lazy","lean",
  // 0x70–0x7F
  "light","linear","liquid","living","locked","lone","long","loud",
  "lower","lucky","lunar","magic","marble","mellow","midnight","mighty",
  // 0x80–0x8F
  "mild","mirrored","misty","mobile","modular","muted","mystic","narrow",
  "native","neon","nested","neutral","noble","north","null","oblique",
  // 0x90–0x9F
  "obscure","odd","offline","onyx","open","outer","pale","parsed",
  "passive","patched","pearl","pending","phantom","plain","polar","proud",
  // 0xA0–0xAF
  "purple","quiet","rapid","rare","raw","regal","remote","rigid",
  "rising","rogue","rooted","rough","royal","rugged","runic","rustic",
  // 0xB0–0xBF
  "rusty","sacred","scarlet","sealed","sharp","sheer","short","silent",
  "silver","simple","skeletal","slow","small","smooth","soft","solar",
  // 0xC0–0xCF
  "solid","south","spare","sparse","spectral","stark","static","steel",
  "still","stone","storm","strange","strict","subtle","swift","tall",
  // 0xD0–0xDF
  "tested","thin","tiny","tidal","tired","total","twisted","unsigned",
  "upper","urban","valid","vast","veiled","verdant","violet","vivid",
  // 0xE0–0xEF
  "void","volatile","warm","wild","winter","wooden","yellow","zealous",
  "zero","blunt","buried","chilled","circular","civic","coiled","cracked",
  // 0xF0–0xFF
  "curved","dented","eroded","formal","frail","glitch","gravel","hazed",
  "hewn","inked","laced","liminal","looping","masked","mossy","nomadic",
];

const NOUNS = [
  // 0x00–0x0F
  "Aardvark","Albatross","Alligator","Alpaca","Anaconda","Antelope","Armadillo","Axolotl",
  "Baboon","Badger","Barracuda","Bat","Bear","Beaver","Bison","Boar",
  // 0x10–0x1F
  "Bobcat","Buffalo","Bullfrog","Butterfly","Caiman","Camel","Capybara","Caracal",
  "Caribou","Catfish","Centipede","Chameleon","Cheetah","Chipmunk","Cobra","Condor",
  // 0x20–0x2F
  "Cormorant","Coyote","Crane","Crocodile","Crow","Deer","Dingo","Dolphin",
  "Dormouse","Dragonfly","Duck","Eagle","Eel","Egret","Elephant","Elk",
  // 0x30–0x3F
  "Emu","Falcon","Ferret","Firefly","Flamingo","Fox","Frog","Gazelle",
  "Gecko","Gerbil","Gibbon","Giraffe","Gnu","Gorilla","Groundhog","Grouse",
  // 0x40–0x4F
  "Gull","Hamster","Hare","Hawk","Hedgehog","Heron","Hippo","Horse",
  "Hornet","Hummingbird","Hyena","Ibex","Iguana","Impala","Jackal","Jaguar",
  // 0x50–0x5F
  "Jellyfish","Kangaroo","Kestrel","Kingfisher","Koala","Komodo","Kookaburra","Lemur",
  "Leopard","Lion","Lizard","Llama","Lobster","Lynx","Macaw","Magpie",
  // 0x60–0x6F
  "Mamba","Manatee","Mandrill","Marmot","Meerkat","Mongoose","Moose","Mule",
  "Narwhal","Newt","Nighthawk","Numbat","Ocelot","Octopus","Okapi","Orangutan",
  // 0x70–0x7F
  "Osprey","Ostrich","Otter","Owl","Panda","Panther","Parrot","Peacock",
  "Pelican","Penguin","Pika","Platypus","Porcupine","Puffin","Puma","Python",
  // 0x80–0x8F
  "Quail","Quetzal","Rabbit","Raccoon","Rattlesnake","Raven","Ray","Reindeer",
  "Rhino","Roadrunner","Robin","Salamander","Salmon","Scorpion","Seahorse","Seal",
  // 0x90–0x9F
  "Serval","Shark","Shoebill","Skunk","Sloth","Snail","Snake","Sparrow",
  "Spider","Squid","Squirrel","Stingray","Stork","Swallow","Swan","Tapir",
  // 0xA0–0xAF
  "Tarsier","Tiger","Toad","Toucan","Turtle","Viper","Vulture","Walrus",
  "Warthog","Wasp","Weasel","Whale","Wildcat","Wolf","Wolverine","Wombat",
  // 0xB0–0xBF
  "Woodpecker","Yak","Zebra","Adder","Agouti","Albacore","Archerfish","Argali",
  "Asp","Basilisk","Bongo","Booby","Bushbuck","Cassowary","Cicada","Civet",
  // 0xC0–0xCF
  "Coati","Cougar","Cuttlefish","Dhole","Dugong","Finch","Flounder","Genet",
  "Gerenuk","Godwit","Goshawk","Guanaco","Guillemot","Guppy","Hoopoe","Hyrax",
  // 0xD0–0xDF
  "Ibis","Kite","Kiwi","Lamprey","Loris","Mackerel","Manta","Marlin",
  "Mink","Mockingbird","Muntjac","Muskrat","Nightjar","Nuthatch","Olm","Oriole",
  // 0xE0–0xEF
  "Oryx","Peccary","Petrel","Pheasant","Pigeon","Piranha","Plover","Pronghorn",
  "Sandpiper","Springbok","Starling","Sturgeon","Sunfish","Swift","Tenrec","Terrapin",
  // 0xF0–0xFF
  "Thrush","Tilapia","Warbler","Wren","Zebu","Zorilla","Tanager","Tamarin",
  "Takin","Curlew","Dace","Drongo","Dunlin","Eland","Ermine","Quokka",
];

// bech32 character-to-value map (alphabet: qpzry9x8gf2tvdw0s3jn54khce6mua7l)
const BECH32_CHARSET = 'qpzry9x8gf2tvdw0s3jn54khce6mua7l';

/**
 * Decode the payload bytes from a bech32/bech32m string.
 * Strips the HRP+separator prefix and 6-char checksum suffix, then converts
 * the 5-bit groups to a byte array.
 * @param {string} addr  e.g. "loot1q…"
 * @returns {number[]|null}  decoded bytes, or null on error
 */
function bech32ToBytes(addr) {
  const sepIdx = addr.lastIndexOf('1');
  if (sepIdx < 1) return null;
  const data = addr.slice(sepIdx + 1, -6); // strip HRP+sep and 6-char checksum
  let bits = 0, bitCount = 0;
  const bytes = [];
  for (const c of data) {
    const val = BECH32_CHARSET.indexOf(c);
    if (val === -1) return null;
    bits = (bits << 5) | val;
    bitCount += 5;
    if (bitCount >= 8) {
      bytes.push((bits >> (bitCount - 8)) & 0xff);
      bitCount -= 8;
    }
  }
  return bytes;
}

/**
 * Convert a lootcoin address to a human-readable wordcode.
 * Accepts bech32m addresses (loot1…) and legacy hex addresses.
 * @param {string} addr  Full address string.
 * @returns {string}  e.g. "silent-Walrus-07421"
 */
export function addressToWordcode(addr) {
  if (!addr || addr.length < 8) return addr ?? "";
  let b0, b1, num;
  if (addr.startsWith('loot1')) {
    const bytes = bech32ToBytes(addr);
    if (!bytes || bytes.length < 4) return addr;
    b0  = bytes[0];
    b1  = bytes[1];
    num = (bytes[2] << 8) | bytes[3];
  } else {
    // Legacy hex fallback
    b0  = Number.parseInt(addr.slice(0, 2), 16);
    b1  = Number.parseInt(addr.slice(2, 4), 16);
    num = Number.parseInt(addr.slice(4, 8), 16);
  }
  return `${ADJECTIVES[b0]}-${NOUNS[b1]}-${String(num).padStart(5, "0")}`;
}
