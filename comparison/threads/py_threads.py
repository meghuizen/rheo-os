# Baselines: Python threading (OS threads behind the GIL) and asyncio
# (userspace coroutine tasks).
import threading, time, asyncio


def bench_threads():
    N = 20_000
    # warm up
    for _ in range(500):
        threading.Thread(target=lambda: None).start()
    t = time.perf_counter()
    ths = [threading.Thread(target=lambda: None) for _ in range(N)]
    for x in ths:
        x.start()
    for x in ths:
        x.join()
    e = time.perf_counter() - t
    print(f"python threading spawn+join: {e/N*1e9:8.1f} ns/thread {N/e/1e6:8.3f} M/s")


async def _amain():
    M = 200_000

    async def blank():
        return 0

    t = time.perf_counter()
    await asyncio.gather(*[blank() for _ in range(M)])
    e = time.perf_counter() - t
    print(f"python asyncio spawn+run   : {e/M*1e9:8.1f} ns/task   {M/e/1e6:8.3f} M/s")


if __name__ == "__main__":
    bench_threads()
    asyncio.run(_amain())
