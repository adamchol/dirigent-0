// What runs, for whoever debugs it (and their bug reports): the web's version, control's
// (`GET /about`: its runner, the players' VM image and system, Postgres's and ClickHouse's
// versions, the install's pods in Kubernetes), the user's hosts, and the session open: its
// player's version and where it runs. All of it copies as JSON.

import { useEffect, useState } from "react";
import { CopyIcon } from "lucide-react";
import { toast } from "sonner";

import { Button } from "@/components/ui/button";
import { Dialog, DialogClose, DialogContent, DialogDescription, DialogFooter, DialogHeader, DialogTitle } from "@/components/ui/dialog";
import { Spinner } from "@/components/ui/spinner";
import * as api from "./api.ts";
import type { About, Host, SessionInfo, Who } from "./api.ts";
import type { LiveStatus } from "./Session.tsx";
import { systemText } from "./Settings.tsx";

type Read<T> = { ok: T } | { error: string } | null;

function read<T>(p: Promise<T>, set: (r: Read<T>) => void) {
  set(null);
  p.then(
    (ok) => set({ ok }),
    (e: Error) => set({ error: e.message }),
  );
}

export function AboutDialog(props: { open: boolean; who: Who; session?: SessionInfo; status: LiveStatus | null; close: () => void }) {
  const [web, setWeb] = useState<Read<string>>(null);
  const [about, setAbout] = useState<Read<About>>(null);
  const [hosts, setHosts] = useState<Read<Host[]>>(null);
  useEffect(() => {
    if (!props.open) return;
    read(api.webVersion(), setWeb);
    read(api.about(props.who), setAbout);
    read(
      api.environments(props.who).then((e) => e.hosts),
      setHosts,
    );
  }, [props.open, props.who]);

  const session = props.session && {
    id: props.session.id,
    placed: props.session.environment ?? "the cluster",
    player: props.session.player?.subject ?? null,
    epoch: props.session.epoch,
    attached: props.session.player?.attached ?? false,
    version: props.status?.version ?? null,
    environment: props.status?.environment ?? null,
    live: props.status ? (props.status.error ? `${props.status.log}: ${props.status.error}` : props.status.log) : null,
  };

  function copy() {
    const value = <T,>(r: Read<T>) => (r === null ? null : "ok" in r ? r.ok : { error: r.error });
    const all = {
      web: value(web),
      about: value(about),
      hosts: value(hosts),
      session: session ?? null,
      origin: location.origin,
      browser: navigator.userAgent,
    };
    navigator.clipboard.writeText(JSON.stringify(all, null, 2)).then(
      () => toast.success("Debug info copied"),
      (e: Error) => toast.error("Not copied", { description: e.message }),
    );
  }

  const a = about && "ok" in about ? about.ok : null;
  return (
    <Dialog open={props.open} onOpenChange={(o) => !o && props.close()}>
      <DialogContent className="sm:max-w-2xl">
        <DialogHeader>
          <DialogTitle>About dirigent</DialogTitle>
          <DialogDescription>What runs, for bug reports: copy it all with the button below.</DialogDescription>
        </DialogHeader>
        <div className="max-h-[60svh] overflow-y-auto">
          <dl className="grid grid-cols-[max-content_minmax(0,1fr)] gap-x-4 gap-y-1.5 text-xs">
            <Row name="Web">{shown(web, (v) => v)}</Row>
            {about && "error" in about && <Row name="Control">couldn't read: {about.error}</Row>}
            {!about && <Row name="Control"><Spinner /></Row>}
            {a && (
              <>
                <Row name="Control">
                  {a.control.version} · started {new Date(a.control.started).toLocaleString()} · runner {a.control.runner ?? "none"}
                </Row>
                <Row name="Players">
                  VM {a.players.vm}
                  {a.players.system && (
                    <>
                      {" · "}
                      {systemText(a.players.system)}
                      {a.players.system.store_path && <span className="block break-all text-dim">{a.players.system.store_path}</span>}
                    </>
                  )}
                </Row>
                <Row name="Postgres">{a.postgres ?? "unknown"}</Row>
                <Row name="ClickHouse">{a.clickhouse ?? "unknown"}</Row>
                <Row name="Pods">
                  {a.pods === null ? (
                    "none: not in Kubernetes"
                  ) : (
                    <ul className="flex flex-col gap-1">
                      {a.pods.map((p) => (
                        <li key={p.name}>
                          <span className="text-strong">{p.component}</span> {p.name} ·{" "}
                          <span className={p.ready ? "text-success" : "text-destructive"}>{p.ready ? "ready" : p.phase.toLowerCase()}</span>
                          {p.restarts > 0 && ` · ${p.restarts} ${p.restarts === 1 ? "restart" : "restarts"}`}
                          <span className="block break-all text-dim">{p.images.join(" ")}</span>
                        </li>
                      ))}
                    </ul>
                  )}
                </Row>
                {a.errors.map((e) => (
                  <Row key={e} name="Couldn't read">
                    <span className="text-destructive">{e}</span>
                  </Row>
                ))}
              </>
            )}
            <Row name="Your hosts">
              {shown(hosts, (hs) =>
                hs.length === 0 ? (
                  "none connected"
                ) : (
                  <ul className="flex flex-col gap-1">
                    {hs.map((h) => (
                      <li key={h.name}>
                        {api.hostLine(h)} · {h.version}
                      </li>
                    ))}
                  </ul>
                ),
              )}
            </Row>
            {session && (
              <Row name="This session">
                {session.id}
                <span className="block text-dim">
                  {session.player
                    ? `${session.player} · epoch ${session.epoch}${session.attached ? "" : " · away"}${session.version ? ` · ${session.version}` : ""}${session.environment ? ` · on ${session.environment}` : ""}`
                    : "idle"}
                  {` · placed on ${session.placed}`}
                  {session.live && ` · log ${session.live}`}
                </span>
              </Row>
            )}
          </dl>
        </div>
        <DialogFooter>
          <Button type="button" variant="outline" onClick={copy}>
            <CopyIcon /> Copy debug info
          </Button>
          <DialogClose render={<Button type="button" />}>Done</DialogClose>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  );
}

function Row(props: { name: string; children: React.ReactNode }) {
  return (
    <>
      <dt className="text-muted-foreground">{props.name}</dt>
      <dd className="min-w-0 text-foreground">{props.children}</dd>
    </>
  );
}

function shown<T>(r: Read<T>, ok: (v: T) => React.ReactNode): React.ReactNode {
  if (r === null) return <Spinner />;
  return "ok" in r ? ok(r.ok) : `couldn't read: ${r.error}`;
}
