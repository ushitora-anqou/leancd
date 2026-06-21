{{/*
Common labels applied to every resource the chart owns.
*/}}
{{- define "leancd.labels" -}}
app.kubernetes.io/name: {{ .Chart.Name }}
app.kubernetes.io/instance: {{ .Release.Name }}
app.kubernetes.io/version: {{ .Chart.AppVersion | quote }}
app.kubernetes.io/managed-by: {{ .Release.Service }}
app.kubernetes.io/part-of: leancd
{{- end -}}

{{/*
Selector labels — the minimal set the ReplicaSet uses to select Pods.
Kept to app.kubernetes.io/name only, for parity with the former
deploy/leancd.yaml and compatibility with the e2e selector
`app.kubernetes.io/name=leancd` (adding the usual instance label would break it).
*/}}
{{- define "leancd.selectorLabels" -}}
app.kubernetes.io/name: {{ .Chart.Name }}
{{- end -}}

{{/* The namespace leancd runs in. */}}
{{- define "leancd.namespace" -}}
{{- .Values.namespace.name | default "leancd" -}}
{{- end -}}

{{/*
The ServiceAccount name leancd uses. When not creating one, default to the
namespace's default account unless an explicit name is given.
*/}}
{{- define "leancd.serviceAccountName" -}}
{{- if .Values.serviceAccount.create -}}
{{- .Values.serviceAccount.name | default "leancd" -}}
{{- else -}}
{{- .Values.serviceAccount.name | default "default" -}}
{{- end -}}
{{- end -}}
