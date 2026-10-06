// The order the engines take within each round of samples. Each sample is preceded by the
// deletion of the previous sample's profile, and the engine that follows pays for it: PGlite's
// profile holds a pool of 1,000 files, and in a fixed cyclic rotation the engine after it, in
// two rounds of three, was always TinyJoin, whose commits then ran two- to threefold slower for
// the first seconds of its sample. The first rounds therefore use every arrangement of the
// engines once, so that within them each engine follows each other engine equally often and
// consecutive rounds start with different engines, which spreads gradual changes in machine load
// equally too. Rounds beyond those take the rotations of the engines' given order in turn, and
// cannot be balanced: the arrangements of three engines take six rounds, so the nine rounds of a
// published run repeat three of them, and counting the pairs that span round boundaries too,
// each engine follows each other engine three to five times in those nine rounds. Rotations of
// the order's reverse would place the repeated pairs elsewhere, but with the boundary pairs
// counted they are less even still.

const permutations = (items) =>
  items.length === 0
    ? [[]]
    : items.flatMap((item, index) => permutations([...items.slice(0, index), ...items.slice(index + 1)]).map((rest) => [item, ...rest]));

const rotations = (order) => order.map((_, shift) => order.map((_, index) => order[(index + shift) % order.length]));

// The arrangements are grouped as the rotations of each arrangement that starts with the first
// engine, so that consecutive rounds start with different engines.
export const roundOrders = (engines, rounds) => {
  if (engines.length === 0) return Array.from({length: rounds}, () => []);
  const [first, ...rest] = engines;
  const arrangements = permutations(rest).flatMap((others) => rotations([first, ...others]));
  const remaining = rotations(engines);
  return Array.from({length: rounds}, (_, round) =>
    round < arrangements.length ? arrangements[round] : remaining[(round - arrangements.length) % remaining.length],
  );
};
