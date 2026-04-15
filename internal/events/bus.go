package events

import "sync"

// Bus is a synchronous fan-out publisher. Subscribers are invoked serially
// under a read lock; a panicking subscriber is recovered so one bad sink
// cannot kill an in-flight engine run.
type Bus struct {
	mu   sync.RWMutex
	subs []Subscriber
}

// NewBus returns an empty Bus.
func NewBus() *Bus {
	return &Bus{}
}

// Subscribe registers a subscriber. Order of subscription determines
// delivery order.
func (b *Bus) Subscribe(s Subscriber) {
	b.mu.Lock()
	defer b.mu.Unlock()
	b.subs = append(b.subs, s)
}

// Publish dispatches the event to every subscriber synchronously.
// Panics from a subscriber are recovered.
func (b *Bus) Publish(e Event) {
	b.mu.RLock()
	subs := make([]Subscriber, len(b.subs))
	copy(subs, b.subs)
	b.mu.RUnlock()

	for _, s := range subs {
		safeHandle(s, e)
	}
}

func safeHandle(s Subscriber, e Event) {
	defer func() {
		_ = recover()
	}()
	s.Handle(e)
}
