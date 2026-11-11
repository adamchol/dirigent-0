// The page: a top bar with the session picked (its id in the URL's hash) and its controls, the
// sessions control has (the sidebar), the session picked, whose log is read live through chlive,
// and a status bar. Who looks depends on control's auth: in `declared`
// (dev), the user chosen in the account menu; in `oidc`, whoever signed in with Google.

import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { LiveClient, defaultUrl } from "@chlive/client";
import { LiveClientProvider } from "@chlive/client/react";
import { AnimatePresence, motion } from "motion/react";
import { ChevronsUpDownIcon, GitPullRequestIcon, KeyRoundIcon, LinkIcon, LockKeyholeIcon, LogOutIcon, MenuIcon, MonitorIcon, MoonIcon, PlusIcon, SunIcon, UserRoundPenIcon, WaypointsIcon } from "lucide-react";

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
import { Sheet, SheetContent, SheetTitle } from "@/components/ui/sheet";
import { Toaster } from "@/components/ui/sonner";
import { Spinner } from "@/components/ui/spinner";
import { TooltipProvider } from "@/components/ui/tooltip";
import { ThemeProvider, useTheme, type Theme } from "@/lib/theme";
import { cn } from "@/lib/utils";
import * as api from "./api.ts";
import type { SessionInfo, Who } from "./api.ts";
import { ChatGptDialog, useMe } from "./Link.tsx";
import { useGoogle } from "./Login.tsx";
import { Session, Viewers, type LiveStatus } from "./Session.tsx";
import { RenameDialog, useTitles } from "./Rename.tsx";
import { SyncDialog } from "./Sync.tsx";
import { AnthropicKeyDialog } from "./Keys.tsx";
import { connectGithub, GithubDialog, useGithubReturn } from "./GitHub.tsx";
import { SecretsDialog } from "./Secrets.tsx";
import { DeleteDialog, NewSession, SessionMenu, SessionsSidebar, useSessions } from "./Sessions.tsx";
import { tokens } from "./Timeline.tsx";

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
        <Alert variant="destructive" className="max-w-md border-0 bg-panel">
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
  return <div className="flex min-h-svh items-center justify-center bg-background p-6">{children}</div>;
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

function remember(key: string, value: string) {
  try {
    localStorage.setItem(key, value);
  } catch {
    // a private window: not remembered
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
    remember("dirigent.user", u);
  }
  return <Page who={who} become={become} />;
}

/** `oidc` auth: whoever signed in with Google. */
function SignedIn(props: { clientId: string }) {
  const google = useGoogle(props.clientId);
  if (google.checking)
    return (
      <Centered>
        <Spinner className="size-5 text-muted-foreground" />
      </Centered>
    );
  if (!google.who)
    return (
      <Centered>
        <motion.div initial={{ opacity: 0, y: 12 }} animate={{ opacity: 1, y: 0 }} transition={{ duration: 0.4, ease: [0.23, 1, 0.32, 1] }}>
          <Card className="w-full max-w-sm border-0 bg-panel ring-0">
            <CardHeader className="items-center text-center">
              <CardTitle className="text-lg text-strong">Sign in to dirigent</CardTitle>
              <CardDescription>With your Google account, to see your sessions.</CardDescription>
            </CardHeader>
            <CardContent className="flex flex-col items-center gap-4">
              <div ref={google.button} className="min-h-10" />
              {google.error && (
                <Alert variant="destructive" className="border-0 bg-background">
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
  const [browsing, setBrowsing] = useState(false);
  const [linking, setLinking] = useState(false);
  const [renaming, setRenaming] = useState<string | null>(null);
  const [deleting, setDeleting] = useState<string | null>(null);
  const [syncing, setSyncing] = useState<string | null>(null);
  const [keying, setKeying] = useState(false);
  // the user's secrets for all sessions (`""`), or a session's (its id)
  const [secretsOf, setSecretsOf] = useState<string | null>(null);
  // the name the secrets dialog starts with (GITHUB_TOKEN, asked for a private repository)
  const [secretName, setSecretName] = useState<string | undefined>();
  const [githubOpen, setGithubOpen] = useState(false);
  // counted up as the user's access to GitHub may have changed
  const [access, setAccess] = useState(0);
  const accessChanged = useCallback(() => setAccess((n) => n + 1), []);
  useGithubReturn(accessChanged);
  const addToken = () => {
    setSecretName("GITHUB_TOKEN");
    setSecretsOf("");
  };
  const [status, setStatus] = useState<LiveStatus | null>(null);
  const client = useClient(props.who);
  const sessions = useSessions(props.who);
  // read again once the ChatGPT dialog closes: it may have been linked meanwhile
  const { me, refresh: refreshMe } = useMe(props.who, 30000);
  useEffect(() => {
    if (!linking) refreshMe();
  }, [linking, refreshMe]);
  const info = sessions.list.find((s) => s.id === selected);
  // the status bar shows the session open; another one reports its own
  useEffect(() => setStatus(null), [selected]);
  const report = useCallback((s: LiveStatus) => setStatus(s), []);
  // the sidebar reads chlive too (when each session was last active)
  if (!client) return null;

  const sidebar = (className?: string) => (
    <SessionsSidebar
      sessions={sessions}
      selected={selected}
      select={(id) => {
        select(id);
        setBrowsing(false);
      }}
      who={props.who}
      create={() => setCreating(true)}
      rename={setRenaming}
      remove={setDeleting}
      openLocally={setSyncing}
      secrets={setSecretsOf}
      account={
        <Account
          who={props.who}
          become={props.become}
          signOut={props.signOut}
          link={() => setLinking(true)}
          keys={() => setKeying(true)}
          secrets={() => setSecretsOf("")}
          github={() => setGithubOpen(true)}
        />
      }
      className={className}
    />
  );

  return (
    <LiveClientProvider client={client}>
      <div className="flex h-svh flex-col bg-background">
        <header className="flex h-9.5 shrink-0 items-stretch bg-panel">
          <button
            type="button"
            className="flex w-10 items-center justify-center text-muted-foreground hover:text-foreground md:hidden"
            onClick={() => setBrowsing(true)}
          >
            <MenuIcon className="size-4" />
            <span className="sr-only">Sessions</span>
          </button>
          <a href="#" className="hidden w-58 shrink-0 items-center px-3.5 font-medium text-strong md:flex">
            dirigent
          </a>
          <SessionTitle info={info} selected={selected} />
          <div className="ml-auto flex shrink-0 items-center gap-1 px-2">
            {info?.role === "owner" && <Viewers info={info} who={props.who} changed={sessions.refresh} />}
            {selected && (
              <SessionMenu
                id={selected}
                info={info}
                who={props.who}
                changed={sessions.refresh}
                rename={() => setRenaming(selected)}
                remove={() => setDeleting(selected)}
                openLocally={() => setSyncing(selected)}
                secrets={() => setSecretsOf(selected)}
              />
            )}
          </div>
        </header>
        <div className="flex min-h-0 flex-1">
          {sidebar("hidden md:flex")}
          <main className="min-w-0 flex-1">
            <AnimatePresence mode="wait" initial={false}>
              <motion.div
                key={`${props.who.user}/${selected}`}
                className="h-full min-w-0"
                initial={{ opacity: 0 }}
                animate={{ opacity: 1 }}
                exit={{ opacity: 0 }}
                transition={{ duration: 0.12 }}
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
                    report={report}
                    me={me}
                    connect={() => setLinking(true)}
                  />
                )}
              </motion.div>
            </AnimatePresence>
          </main>
        </div>
        <StatusBar info={info} status={selected ? status : null} />
      </div>
      <Sheet open={browsing} onOpenChange={setBrowsing}>
        <SheetContent side="left" className="w-64 gap-0 border-0 p-0" showCloseButton={false}>
          <SheetTitle className="sr-only">Sessions</SheetTitle>
          {sidebar("h-full w-full")}
        </SheetContent>
      </Sheet>
      <NewSession
        open={creating}
        setOpen={setCreating}
        who={props.who}
        me={me}
        connect={() => setLinking(true)}
        addToken={addToken}
        connectGithub={() => connectGithub(props.who)}
        again={access}
        made={(id) => {
          select(id);
          sessions.refresh();
        }}
      />
      <RenameDialog
        info={sessions.list.find((s) => s.id === renaming)}
        client={client}
        close={() => setRenaming(null)}
      />
      <DeleteDialog
        info={sessions.list.find((s) => s.id === deleting)}
        who={props.who}
        deleted={(id) => {
          if (id === selected) select("");
          sessions.refresh();
        }}
        close={() => setDeleting(null)}
      />
      <ChatGptDialog who={props.who} declared={!!props.become} open={linking} setOpen={setLinking} />
      <SyncDialog id={syncing} who={props.who} declared={!!props.become} close={() => setSyncing(null)} />
      <AnthropicKeyDialog open={keying} who={props.who} me={me} changed={refreshMe} close={() => setKeying(false)} />
      <GithubDialog
        open={githubOpen}
        who={props.who}
        addToken={() => {
          setGithubOpen(false);
          addToken();
        }}
        close={() => setGithubOpen(false)}
      />
      <SecretsDialog
        open={secretsOf !== null}
        who={props.who}
        session={secretsOf ? sessions.list.find((s) => s.id === secretsOf) : undefined}
        name={secretName}
        close={() => {
          setSecretsOf(null);
          setSecretName(undefined);
          accessChanged();
          sessions.refresh();
        }}
      />
    </LiveClientProvider>
  );
}

/** The session open's title, and what it's about where there's room. */
function SessionTitle({ info, selected }: { info?: SessionInfo; selected: string }) {
  const named = useTitles().get(selected);
  return (
    <h1 className="flex min-w-0 items-baseline gap-2.5 self-center px-3 text-[13px] font-normal md:px-4" title={named?.description}>
      <span className="max-w-full shrink-0 truncate text-strong">{info ? api.title(info, named) : selected ? "Session" : ""}</span>
      {named?.description && <span className="hidden truncate text-muted-foreground lg:inline">{named.description}</span>}
    </h1>
  );
}

/** What's known of the session open: chlive's connection, its player, the turn, the model. */
/** What a starting session waits on, in the status bar. */
const STARTING = { machine: "Starting", workspace: "Workspace", turn: "Starting turn" } as const;

function StatusBar({ info, status }: { info?: SessionInfo; status: LiveStatus | null }) {
  const live = !status
    ? null
    : status.log === "live"
      ? { dot: "bg-success", label: "Live" }
      : status.log === "error"
        ? { dot: "bg-destructive", label: status.error ? `Offline: ${status.error}` : "Offline" }
        : { dot: "bg-primary", label: "Connecting" };
  const player = info?.player;
  return (
    <footer className="flex h-6 shrink-0 items-center gap-4 overflow-hidden bg-panel px-3 text-[11.5px] whitespace-nowrap text-muted-foreground">
      {live && (
        <span className="flex items-center gap-1.5">
          <span aria-hidden="true" className={cn("size-1.5 rounded-full", live.dot)} />
          {live.label}
        </span>
      )}
      {info &&
        (player ? (
          <span>
            Player {player.subject.replace(/^player:/, "").slice(0, 8)} · epoch {info.epoch}
            {player.attached ? "" : " · away"}
          </span>
        ) : (
          <span>Idle</span>
        ))}
      {status?.running && <span className="text-primary-text">Working</span>}
      {status?.startup && !status.running && <span className="text-primary-text">{STARTING[status.startup]}</span>}
      <span className="flex-1" />
      {info && (
        <span className="hidden sm:inline">
          {info.spec.model}
          {info.spec.effort ? ` · ${info.spec.effort}` : ""}
        </span>
      )}
      {status && status.context > 0 && <span className="hidden sm:inline">{tokens(status.context)} context</span>}
    </footer>
  );
}

function Welcome({ create }: { create: () => void }) {
  return (
    <Empty className="h-full">
      <EmptyHeader>
        <EmptyMedia variant="icon" className="bg-panel">
          <WaypointsIcon />
        </EmptyMedia>
        <EmptyTitle className="text-strong">Pick a session</EmptyTitle>
        <EmptyDescription>Or make one: a coder works in a repository, a dirigent directs your coders.</EmptyDescription>
      </EmptyHeader>
      <EmptyContent>
        <Button size="sm" onClick={create}>
          <PlusIcon /> New session
        </Button>
      </EmptyContent>
    </Empty>
  );
}

/** Two letters for a subject: `user:ada.lovelace@…` → AL. */
function initials(user: string): string {
  const name = user.replace(/^[a-z]+:/, "").split("@")[0] ?? "";
  const parts = name.split(/[._-]+/).filter(Boolean);
  return ((parts[0]?.[0] ?? "?") + (parts[1]?.[0] ?? "")).toUpperCase();
}

/** Who looks, and the theme; signing out, or (declared auth) looking as someone else. */
function Account(props: {
  who: Who;
  become?: (user: string) => void;
  signOut?: () => void;
  link: () => void;
  keys: () => void;
  secrets: () => void;
  github: () => void;
}) {
  const { theme, setTheme } = useTheme();
  const [draft, setDraft] = useState(props.who.user);
  useEffect(() => setDraft(props.who.user), [props.who.user]);
  return (
    <DropdownMenu>
      <DropdownMenuTrigger
        render={<button type="button" className="flex w-full items-center gap-2 rounded-[5px] p-1.5 text-left hover:bg-accent aria-expanded:bg-accent" />}
      >
        <Avatar className="size-6 rounded-[5px]">
          <AvatarFallback className="rounded-[5px] bg-secondary text-[10px] text-subtle">{initials(props.who.user)}</AvatarFallback>
        </Avatar>
        <span className="min-w-0 flex-1 truncate text-xs text-subtle">{props.who.user.replace(/^user:/, "")}</span>
        <ChevronsUpDownIcon className="size-3.5 text-dim" />
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
                <InputGroupInput value={draft} onChange={(e) => setDraft(e.target.value)} onKeyDown={(e) => e.stopPropagation()} spellCheck={false} />
                <InputGroupAddon align="inline-end">
                  <InputGroupButton type="submit">Go</InputGroupButton>
                </InputGroupAddon>
              </InputGroup>
            </form>
            <DropdownMenuSeparator />
          </>
        )}
        <DropdownMenuGroup>
          <DropdownMenuItem onClick={props.link}>
            <LinkIcon /> ChatGPT plan
          </DropdownMenuItem>
          <DropdownMenuItem onClick={props.keys}>
            <KeyRoundIcon /> Anthropic API key
          </DropdownMenuItem>
          <DropdownMenuItem onClick={props.secrets}>
            <LockKeyholeIcon /> Secrets
          </DropdownMenuItem>
          <DropdownMenuItem onClick={props.github}>
            <GitPullRequestIcon /> GitHub
          </DropdownMenuItem>
        </DropdownMenuGroup>
        <DropdownMenuSeparator />
        <DropdownMenuGroup>
          <DropdownMenuLabel>Theme</DropdownMenuLabel>
          <DropdownMenuRadioGroup value={theme} onValueChange={(v) => setTheme(v as Theme)}>
            <DropdownMenuRadioItem value="dark">
              <MoonIcon /> Dark
            </DropdownMenuRadioItem>
            <DropdownMenuRadioItem value="light">
              <SunIcon /> Light
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
  );
}
