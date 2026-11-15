// The chat's scroll: it follows the bottom while you're there (a session opens there, whatever
// order its history arrives in, and a reply as it streams), stops following as soon as you
// scroll up, and follows again once you're back at the bottom or press the button.

import * as React from "react"
import { cn } from "cn"

import { Button } from "@/components/ui/button"
import { ArrowDownIcon } from "lucide-react"

/** How close to the bottom counts as at it, in pixels. */
const AT_BOTTOM = 24

type Scroller = {
  viewport: React.RefObject<HTMLDivElement | null>
  content: React.RefObject<HTMLDivElement | null>
  /** Whether the view follows the bottom. */
  following: React.RefObject<boolean>
  /** Whether you're away from the bottom (the button shows). */
  away: boolean
  setAway: (away: boolean) => void
}

const Context = React.createContext<Scroller | null>(null)

function useScroller(): Scroller {
  const scroller = React.useContext(Context)
  if (!scroller) throw new Error("outside a MessageScrollerProvider")
  return scroller
}

function toBottom(viewport: HTMLElement, behavior: ScrollBehavior = "auto") {
  viewport.scrollTo({ top: viewport.scrollHeight, behavior })
}

function MessageScrollerProvider({ children }: { children?: React.ReactNode }) {
  const viewport = React.useRef<HTMLDivElement | null>(null)
  const content = React.useRef<HTMLDivElement | null>(null)
  const following = React.useRef(true)
  const [away, setAway] = React.useState(false)
  const scroller = React.useMemo(
    () => ({ viewport, content, following, away, setAway }),
    [away]
  )
  return <Context.Provider value={scroller}>{children}</Context.Provider>
}

function MessageScroller({ className, ...props }: React.ComponentProps<"div">) {
  return (
    <div
      data-slot="message-scroller"
      className={cn("relative flex size-full min-h-0 flex-col overflow-hidden", className)}
      {...props}
    />
  )
}

function MessageScrollerViewport({ className, onScroll, ...props }: React.ComponentProps<"div">) {
  const { viewport, content, following, setAway } = useScroller()
  const lastTop = React.useRef(0)

  // whatever changes size (the log, a streamed reply, the window), the bottom stays in view
  // while it's followed
  React.useLayoutEffect(() => {
    const view = viewport.current
    const inner = content.current
    if (!view || !inner) return
    const follow = () => {
      if (following.current) toBottom(view)
    }
    follow()
    const observer = new ResizeObserver(follow)
    observer.observe(inner)
    observer.observe(view)
    return () => observer.disconnect()
  }, [viewport, content, following])

  return (
    <div
      ref={viewport}
      data-slot="message-scroller-viewport"
      className={cn(
        "size-full min-h-0 min-w-0 scroll-fade-b scrollbar-thin scrollbar-gutter-stable overflow-y-auto overscroll-contain",
        className
      )}
      onScroll={(e) => {
        const view = e.currentTarget
        const gap = view.scrollHeight - view.scrollTop - view.clientHeight
        // at the bottom, it's followed; moved up (by you: following only moves it down, and a
        // log shrinking under it leaves it at the bottom), it isn't any more
        if (gap <= AT_BOTTOM) following.current = true
        else if (view.scrollTop < lastTop.current) following.current = false
        lastTop.current = view.scrollTop
        setAway(gap > AT_BOTTOM && !following.current)
        onScroll?.(e)
      }}
      {...props}
    />
  )
}

function MessageScrollerContent({ className, ...props }: React.ComponentProps<"div">) {
  const { content } = useScroller()
  return (
    <div
      ref={content}
      data-slot="message-scroller-content"
      className={cn("flex h-max min-h-full flex-col gap-6", className)}
      {...props}
    />
  )
}

function MessageScrollerItem({ className, ...props }: React.ComponentProps<"div">) {
  return <div data-slot="message-scroller-item" className={cn("min-w-0 shrink-0", className)} {...props} />
}

/** Back to the bottom, which is then followed again; shown while you're away from it. */
function MessageScrollerButton({ className, ...props }: React.ComponentProps<typeof Button>) {
  const { viewport, following, away, setAway } = useScroller()
  return (
    <Button
      data-slot="message-scroller-button"
      data-active={away}
      variant="secondary"
      size="icon-sm"
      tabIndex={away ? 0 : -1}
      className={cn(
        "absolute start-1/2 bottom-4 -translate-x-1/2 border-border bg-background text-foreground transition-[translate,scale,opacity] duration-200 hover:bg-muted hover:text-foreground data-[active=false]:pointer-events-none data-[active=false]:translate-y-full data-[active=false]:scale-95 data-[active=false]:opacity-0 data-[active=true]:translate-y-0 data-[active=true]:scale-100 data-[active=true]:opacity-100 rtl:translate-x-1/2",
        className
      )}
      onClick={() => {
        following.current = true
        setAway(false)
        if (viewport.current) toBottom(viewport.current, "smooth")
      }}
      {...props}
    >
      <ArrowDownIcon />
      <span className="sr-only">Scroll to end</span>
    </Button>
  )
}

export {
  MessageScrollerProvider,
  MessageScroller,
  MessageScrollerViewport,
  MessageScrollerContent,
  MessageScrollerItem,
  MessageScrollerButton,
}
