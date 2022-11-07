const child_process = require("child_process");

const RUNS = 5;
/**
 * @type child_process.ChildProcess[]
 */
const children = [];

for (let i = 0; i < RUNS; i++) {
  children.push(
    child_process.spawn(`/usr/bin/sh`, [
      `-c`,
      `"while ./target/release/uffd-vm-test &> output-test${i}.log; do echo succeeded; done"`,
    ])
  );
}

// Now we wait
setInterval(() => {
  const killedChildren = children.filter((child) => child.killed);

  if (killedChildren.length > 0) {
    console.log("A child is killed: " + killedChildren[0].spawnargs);
    process.exit(1);
  }
}, 500);
