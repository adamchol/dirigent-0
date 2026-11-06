// The page: the sessions control has (the sidebar), and the one picked (its id in the URL's
// hash), whose log is read live through chlive. Who looks depends on control's auth: in
// `declared` (dev), the user chosen in the account menu; in `oidc`, whoever signed in with Google.

import { useEffect, useMemo, useRef, useState } from "react";
import { LiveClient, defaultUrl } from "@chlive/client";
import { LiveClientProvider } from "@chlive/client/react";
import { AnimatePresence, motion } from "motion/react";
import { ChevronsUpDownIcon, LogOutIcon, MonitorIcon, MoonIcon, PlusIcon, SunIcon, UserRoundPenIcon, WaypointsIcon } from "lucide-react";

import { Alert, AlertDescription, AlertTitle } from "@/components/ui/alert";
import { Avatar, AvatarFallback } from "@/components/ui/avatar";
import { Button } from "@/components/ui/button";
import { Card, CardContent, CardDescription, CardHeader, CardTitle } from "@/components/ui/card";
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuGroup,
  DropdownMenuItem,
  DropdownMenuLabel,
  DropdownMenuRadioGroup,
  DropdownMenuRadioItem,
  DropdownMenuSeparator,
  DropdownMenuTrigger,
} from "@/components/ui/dropdown-menu";
import { Empty, EmptyContent, EmptyDescription, EmptyHeader, EmptyMedia, EmptyTitle } from "@/components/ui/empty";
import { InputGroup, InputGroupAddon, InputGroupButton, InputGroupInput } from "@/components/ui/input-group";
import { SidebarInset, SidebarMenu, SidebarMenuButton, SidebarMenuItem, SidebarProvider, SidebarTrigger } from "@/components/ui/sidebar";
import { Toaster } from "@/components/ui/sonner";
import { Spinner } from "@/components/ui/spinner";
import { TooltipProvider } from "@/components/ui/tooltip";
import { ThemeProvider, useTheme, type Theme } from "@/lib/theme";
import * as api from "./api.ts";
import type { Who } from "./api.ts";
import { useGoogle } from "./Login.tsx";
import logo from "./logo.svg";
import { Session } from "./Session.tsx";
import { NewSession, SessionsSidebar, useSessions } from "./Sessions.tsx";

export function App() {
  return (
    <ThemeProvider>
      <TooltipProvider>
        <Root />
        <Toaster position="bottom-right" />
      </TooltipProvider>
    </ThemeProvider>
  );
}

function Root() {
  const [auth, setAuth] = useState<api.Auth | null>(null);
  const [error, setError] = useState("");
  useEffect(() => {
    api.auth().then(setAuth, (e) => setError((e as Error).message));
  }, []);
  if (error)
    return (
      <Centered>
        <Alert variant="destructive" className="max-w-md">
          <AlertTitle>Control is unreachable</AlertTitle>
          <AlertDescription>{error}</AlertDescription>
        </Alert>
      </Centered>
    );
  if (!auth)
    return (
      <Centered>
        <Spinner className="size-5 text-muted-foreground" />
      </Centered>
    );
  return auth.mode === "oidc" ? <SignedIn clientId={auth.google_client_id} /> : <Declared />;
}

function Centered({ children }: { children: React.ReactNode }) {
  return <div className="flex min-h-svh items-center justify-center bg-muted/30 p-6">{children}</div>;
}

/** A connection to chlive as `who`, closed when they change (not when their token's renewed:
 * the client asks for the latest one). */
function useClient(who: Who): LiveClient | null {
  const latest = useRef(who);
  latest.current = who;
  const [client, setClient] = useState<LiveClient | null>(null);
  useEffect(() => {
    const c = new LiveClient({
      url: defaultUrl(),
      user: who.user,
      token: () => api.token(latest.current),
      cache: false,
      // the user's messages show at once, placed by chlive-core (WebAssembly) where chlive will
      // put them
      optimistic: true,
    });
    setClient(c);
    return () => c.close();
  }, [who.user]);
  return client;
}

function useHash(): [string, (h: string) => void] {
  const read = () => decodeURIComponent(location.hash.slice(1));
  const [hash, setHash] = useState(read);
  useEffect(() => {
    const changed = () => setHash(read());
    addEventListener("hashchange", changed);
    return () => removeEventListener("hashchange", changed);
  }, []);
  return [hash, (h) => (location.hash = h)];
}

function remembered(key: string, fallback: string): string {
  try {
    return localStorage.getItem(key) ?? fallback;
  } catch {
    return fallback;
  }
}

/** `declared` auth: whoever the account menu says. */
function Declared() {
  const [user, setUser] = useState(() => remembered("dirigent.user", "user:dev"));
  const who = useMemo(() => ({ user }), [user]);
  function become(to: string) {
    const u = to.trim();
    if (!u) return;
    setUser(u);
    try {
      localStorage.setItem("dirigent.user", u);
    } catch {
      // a private window: not remembered
    }
  }
  return <Page who={who} become={become} />;
}

/** `oidc` auth: whoever signed in with Google. */
function SignedIn(props: { clientId: string }) {
  const google = useGoogle(props.clientId);
  if (!google.who)
    return (
      <Centered>
        <motion.div initial={{ opacity: 0, y: 12 }} animate={{ opacity: 1, y: 0 }} transition={{ duration: 0.4, ease: [0.23, 1, 0.32, 1] }}>
          <Card className="w-full max-w-sm">
            <CardHeader className="items-center text-center">
              <img src={logo} alt="" className="mx-auto mb-2 size-12 rounded-xl" />
              <CardTitle className="text-xl">Sign in to dirigent</CardTitle>
              <CardDescription>With your Google account, to see your sessions.</CardDescription>
            </CardHeader>
            <CardContent className="flex flex-col items-center gap-4">
              <div ref={google.button} className="min-h-10" />
              {google.error && (
                <Alert variant="destructive">
                  <AlertDescription>{google.error}</AlertDescription>
                </Alert>
              )}
            </CardContent>
          </Card>
        </motion.div>
      </Centered>
    );
  return <Page who={google.who} signOut={google.signOut} />;
}

function Page(props: { who: Who; become?: (user: string) => void; signOut?: () => void }) {
  const [selected, select] = useHash();
  const [creating, setCreating] = useState(false);
  const client = useClient(props.who);
  const sessions = useSessions(props.who);
  const info = sessions.list.find((s) => s.id === selected);
  // the sidebar reads chlive too (when each session was last active)
  if (!client) return null;

  return (
    <LiveClientProvider client={client}>
      <SidebarProvider>
        <SessionsSidebar
          sessions={sessions}
          selected={selected}
          select={select}
          who={props.who}
          create={() => setCreating(true)}
          account={<Account who={props.who} become={props.become} signOut={props.signOut} />}
        />
        <SidebarInset className="min-w-0 overflow-hidden">
          <AnimatePresence mode="wait" initial={false}>
            <motion.div
              key={`${props.who.user}/${selected}`}
              className="min-w-0"
              initial={{ opacity: 0 }}
              animate={{ opacity: 1 }}
              exit={{ opacity: 0 }}
              transition={{ duration: 0.15 }}
            >
              {!selected ? (
                <Welcome create={() => setCreating(true)} />
              ) : (
                <Session
                  id={selected}
                  client={client}
                  who={props.who}
                  info={info}
                  listed={sessions.listed}
                  become={props.become}
                  changed={sessions.refresh}
                />
              )}
            </motion.div>
          </AnimatePresence>
        </SidebarInset>
        <NewSession
          open={creating}
          setOpen={setCreating}
          who={props.who}
          made={(id) => {
            select(id);
            sessions.refresh();
          }}
        />
      </SidebarProvider>
    </LiveClientProvider>
  );
}

function Welcome({ create }: { create: () => void }) {
  return (
    <div className="flex h-svh flex-col">
      <header className="flex h-14 shrink-0 items-center px-3">
        <SidebarTrigger />
      </header>
      <Empty className="flex-1">
        <EmptyHeader>
          <EmptyMedia variant="icon">
            <WaypointsIcon />
          </EmptyMedia>
          <EmptyTitle>Pick a session</EmptyTitle>
          <EmptyDescription>
            Or make one: a coder works in a repository, a dirigent directs your coders.
          </EmptyDescription>
        </EmptyHeader>
        <EmptyContent>
          <Button onClick={create}>
            <PlusIcon /> New session
          </Button>
        </EmptyContent>
      </Empty>
    </div>
  );
}

/** Two letters for a subject: `user:ada.lovelace@…` → AL. */
function initials(user: string): string {
  const name = user.replace(/^[a-z]+:/, "").split("@")[0] ?? "";
  const parts = name.split(/[._-]+/).filter(Boolean);
  return ((parts[0]?.[0] ?? "?") + (parts[1]?.[0] ?? "")).toUpperCase();
}

/** Who looks, and the theme; signing out, or (declared auth) looking as someone else. */
function Account(props: { who: Who; become?: (user: string) => void; signOut?: () => void }) {
  const { theme, setTheme } = useTheme();
  const [draft, setDraft] = useState(props.who.user);
  useEffect(() => setDraft(props.who.user), [props.who.user]);
  return (
    <SidebarMenu>
      <SidebarMenuItem>
        <DropdownMenu>
          <DropdownMenuTrigger render={<SidebarMenuButton size="lg" />}>
            <Avatar className="size-8 rounded-lg">
              <AvatarFallback className="rounded-lg">{initials(props.who.user)}</AvatarFallback>
            </Avatar>
            <div className="grid flex-1 text-left text-sm leading-tight">
              <span className="truncate font-medium">{props.who.user.replace(/^user:/, "")}</span>
              <span className="truncate text-xs text-muted-foreground">{props.signOut ? "Google" : "declared (dev)"}</span>
            </div>
            <ChevronsUpDownIcon className="ml-auto size-4" />
          </DropdownMenuTrigger>
          <DropdownMenuContent side="top" align="start" className="w-(--anchor-width) min-w-64">
            {props.become && (
              <>
                <DropdownMenuGroup>
                  <DropdownMenuLabel>Look as</DropdownMenuLabel>
                </DropdownMenuGroup>
                <form
                  className="px-1.5 pb-1.5"
                  onSubmit={(e) => {
                    e.preventDefault();
                    props.become!(draft);
                  }}
                >
                  <InputGroup>
                    <InputGroupAddon>
                      <UserRoundPenIcon />
                    </InputGroupAddon>
                    <InputGroupInput
                      value={draft}
                      onChange={(e) => setDraft(e.target.value)}
                      onKeyDown={(e) => e.stopPropagation()}
                      spellCheck={false}
                    />
                    <InputGroupAddon align="inline-end">
                      <InputGroupButton type="submit">Go</InputGroupButton>
                    </InputGroupAddon>
                  </InputGroup>
                </form>
                <DropdownMenuSeparator />
              </>
            )}
            <DropdownMenuGroup>
              <DropdownMenuLabel>Theme</DropdownMenuLabel>
              <DropdownMenuRadioGroup value={theme} onValueChange={(v) => setTheme(v as Theme)}>
                <DropdownMenuRadioItem value="light">
                  <SunIcon /> Light
                </DropdownMenuRadioItem>
                <DropdownMenuRadioItem value="dark">
                  <MoonIcon /> Dark
                </DropdownMenuRadioItem>
                <DropdownMenuRadioItem value="system">
                  <MonitorIcon /> System
                </DropdownMenuRadioItem>
              </DropdownMenuRadioGroup>
            </DropdownMenuGroup>
            {props.signOut && (
              <>
                <DropdownMenuSeparator />
                <DropdownMenuItem onClick={props.signOut}>
                  <LogOutIcon /> Sign out
                </DropdownMenuItem>
              </>
            )}
          </DropdownMenuContent>
        </DropdownMenu>
      </SidebarMenuItem>
    </SidebarMenu>
  );
}
