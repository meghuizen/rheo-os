// Baseline: Go goroutines - the best-in-class userspace light thread (M:N,
// segmented stacks, work-stealing). The fair comparison for strands.
package main

import (
	"fmt"
	"sync"
	"time"
)

func main() {
	const N = 2000000
	var wg sync.WaitGroup
	t := time.Now()
	for i := 0; i < N; i++ {
		wg.Add(1)
		go func() { wg.Done() }()
	}
	wg.Wait()
	e := time.Since(t)
	fmt.Printf("goroutine spawn+run       : %7.1f ns/goroutine %7.2f M/s\n",
		float64(e.Nanoseconds())/float64(N), float64(N)/e.Seconds()/1e6)

	// Context switch: ping-pong over an unbuffered channel.
	const K = 2000000
	c := make(chan struct{})
	done := make(chan struct{})
	t = time.Now()
	go func() {
		for i := 0; i < K; i++ {
			c <- struct{}{}
			<-c
		}
		close(done)
	}()
	for i := 0; i < K; i++ {
		<-c
		c <- struct{}{}
	}
	<-done
	e = time.Since(t)
	sw := float64(2 * K)
	fmt.Printf("goroutine chan switch     : %7.1f ns/switch %7.2f M switch/s\n",
		float64(e.Nanoseconds())/sw, sw/e.Seconds()/1e6)
}
